//! File transfer v1 on the host: the `/host-file-ws` socket, uploads saved to
//! `Downloads\BubblePlay`, and downloads offered from the clipboard or the
//! Send To spool. Intent: `docs/architecture/file-transfer.md`; the protocol and
//! its pure state machines are in `crate::file_transfer`.
//!
//! Threads: one socket thread (connects while a viewer epoch is set, serves
//! one upload and one download at a time) and one watcher thread (clipboard
//! file copies and Send To spool files). The main loop drains signaling
//! messages (`file-offer`) and paste requests with `take_outbox`, and feeds
//! the rate cap.
//!
//! Environment (for tests and the lab):
//! - `BUBBLEPLAY_DOWNLOADS_DIR`: save uploads here instead of
//!   `%USERPROFILE%\Downloads\BubblePlay`.
//! - `BUBBLEPLAY_SENDTO_DIR`: spool directory instead of
//!   `%LOCALAPPDATA%\BubblePlay\sendto`.
//! - `BUBBLEPLAY_FILE_CLIPBOARD=0`: never read or write the clipboard and
//!   never inject a paste (uploads are only saved; offers come from Send To).

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{debug, info, warn};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::file_hash::Sha256;
use crate::file_transfer::{
    self as ft, Body, CancelPayload, FileOfferItem, Incoming, OfferMeta, Packet, Purpose,
    RateLimiter, RequestPayload, ResultPayload, SendWindow, TransferError,
};
use crate::native_file_drop::{DropBatch, NativeDropRequest};
use crate::wss_media::{host_socket_request, HostSocket};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const INBOUND_SILENCE_LIMIT: Duration = Duration::from_secs(40);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(10);
const WATCH_INTERVAL: Duration = Duration::from_millis(300);
/// A Send To request older than this (no viewer connected in time) is dropped.
const SPOOL_MAX_AGE: Duration = Duration::from_secs(10 * 60);
/// Queued download requests per socket.
const MAX_QUEUED_REQUESTS: usize = 64;
/// No ACK progress for this long while bytes are in flight: give up.
const ACK_STALL_LIMIT: Duration = Duration::from_secs(30);
const PARTIAL_SUFFIX: &str = ".bppartial";

#[derive(Clone, Debug)]
pub struct FileServiceConfig {
    pub downloads_dir: PathBuf,
    pub sendto_dir: Option<PathBuf>,
    pub clipboard: bool,
}

impl FileServiceConfig {
    pub fn from_env() -> Self {
        let downloads_dir = std::env::var_os("BUBBLEPLAY_DOWNLOADS_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(default_downloads_dir);
        let sendto_dir = std::env::var_os("BUBBLEPLAY_SENDTO_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(default_sendto_dir);
        let clipboard = std::env::var("BUBBLEPLAY_FILE_CLIPBOARD").map_or(true, |v| v != "0");
        Self {
            downloads_dir,
            sendto_dir,
            clipboard,
        }
    }
}

fn default_downloads_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Downloads")
        .join("BubblePlay")
}

/// `%LOCALAPPDATA%\BubblePlay\sendto` (shared with `bubbleplay_tray --send-files`).
pub fn default_sendto_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(|base| PathBuf::from(base).join("BubblePlay").join("sendto"))
}

/// One Send To request, written by `bubbleplay_tray.exe --send-files`.
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct SpoolRequest {
    pub protocol: u8,
    #[serde(rename = "createdMs")]
    pub created_ms: u64,
    pub paths: Vec<PathBuf>,
}

/// Write a Send To request atomically into `dir` (temp file + rename).
pub fn write_spool_request(dir: &Path, paths: &[PathBuf]) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let request = SpoolRequest {
        protocol: 1,
        created_ms: unix_ms(),
        paths: paths.to_vec(),
    };
    let stem = format!("{}-{}", request.created_ms, std::process::id());
    let temp = dir.join(format!("{stem}.tmp"));
    let target = dir.join(format!("{stem}.json"));
    fs::write(&temp, serde_json::to_vec(&request)?)
        .with_context(|| format!("write {}", temp.display()))?;
    fs::rename(&temp, &target).with_context(|| format!("publish {}", target.display()))?;
    Ok(target)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Debug)]
struct OfferedFile {
    id: u32,
    path: PathBuf,
    name: String,
    size: u64,
}

#[derive(Clone, Debug)]
struct CurrentOffer {
    id: u32,
    source: &'static str,
    items: Vec<OfferedFile>,
}

impl CurrentOffer {
    fn message(&self) -> String {
        let items: Vec<FileOfferItem> = self
            .items
            .iter()
            .map(|item| FileOfferItem {
                id: item.id,
                name: item.name.clone(),
                size: item.size,
            })
            .collect();
        ft::file_offer_message(self.id, &items, self.source)
    }
}

/// Counters for logs and the lab.
#[derive(Clone, Debug, Default)]
pub struct FileStats {
    pub uploads_ok: u64,
    pub uploads_failed: u64,
    pub downloads_ok: u64,
    pub downloads_failed: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

#[derive(Default)]
pub struct FileOutbox {
    /// Signaling text for `/ws` (file-offer).
    pub signals: Vec<String>,
    /// Paste-purpose uploads placed on the clipboard: inject Ctrl+V once each.
    pub pastes: u32,
    /// Native drops await the main loop's active-desktop geometry/session check.
    pub drops: Vec<NativeDropRequest>,
}

struct Shared {
    epoch: Option<String>,
    rate_cap_bps: u64,
    offer: Option<CurrentOffer>,
    next_offer_id: u32,
    outbox: FileOutbox,
    /// Clipboard sequence after our own CF_HDROP write (never offered back).
    own_clipboard_seq: Option<u32>,
    stats: FileStats,
    connected: bool,
}

pub struct FileService {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl FileService {
    pub fn spawn(worker_url: &str, bearer: &str, config: FileServiceConfig) -> Result<Self> {
        let shared = Arc::new(Mutex::new(Shared {
            epoch: None,
            rate_cap_bps: 0,
            offer: None,
            next_offer_id: 1,
            outbox: FileOutbox::default(),
            own_clipboard_seq: None,
            stats: FileStats::default(),
            connected: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        {
            let (shared, stop, config) = (shared.clone(), stop.clone(), config.clone());
            let (url, bearer) = (worker_url.to_owned(), bearer.to_owned());
            threads.push(
                thread::Builder::new()
                    .name("bubbleplay-file-socket".into())
                    .spawn(move || socket_loop(&url, &bearer, &config, &shared, &stop))
                    .context("spawn file socket thread")?,
            );
        }
        {
            let (shared, stop) = (shared.clone(), stop.clone());
            threads.push(
                thread::Builder::new()
                    .name("bubbleplay-file-watch".into())
                    .spawn(move || watch_loop(&config, &shared, &stop))
                    .context("spawn file watcher thread")?,
            );
        }
        info!("file transfer service started");
        Ok(Self {
            shared,
            stop,
            threads,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        lock(&self.shared)
    }

    /// The viewer epoch the file socket pairs on; `None` closes it. A new
    /// epoch re-announces the current offer.
    pub fn set_epoch(&self, epoch: Option<&str>) {
        let mut shared = self.lock();
        if shared.epoch.as_deref() == epoch {
            return;
        }
        shared.epoch = epoch.map(str::to_owned);
        if epoch.is_some() {
            if let Some(message) = shared.offer.as_ref().map(CurrentOffer::message) {
                shared.outbox.signals.push(message);
            }
        }
    }

    /// File bytes per second allowed now (0 = no cap).
    pub fn set_rate_cap_bps(&self, bps: u64) {
        self.lock().rate_cap_bps = bps;
    }

    pub fn take_outbox(&self) -> FileOutbox {
        std::mem::take(&mut self.lock().outbox)
    }

    pub fn stats(&self) -> FileStats {
        self.lock().stats.clone()
    }

    pub fn connected(&self) -> bool {
        self.lock().connected
    }
}

impl Drop for FileService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn sleep_unless_stopped(stop: &AtomicBool, delay: Duration) {
    let deadline = Instant::now() + delay;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

// ---------------------------------------------------------------------------
// Offers: clipboard file copies and Send To.

/// Build offer items from paths: regular files only (directories are skipped
/// in v1), at most 64, each at most 2 GiB.
fn offer_items(paths: &[PathBuf]) -> Vec<OfferedFile> {
    let mut items = Vec::new();
    for path in paths {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                debug!(path = %path.display(), ?error, "offer: unreadable path skipped");
                continue;
            }
        };
        if !metadata.is_file() {
            info!(path = %path.display(), "offer: directory skipped (files only in v1)");
            continue;
        }
        if metadata.len() > ft::MAX_FILE_BYTES {
            info!(path = %path.display(), size = metadata.len(), "offer: file above 2 GiB skipped");
            continue;
        }
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        if name.is_empty() || name.len() > ft::MAX_NAME_BYTES {
            continue;
        }
        if items.len() == ft::MAX_BATCH_FILES as usize {
            info!("offer: more than 64 files; the rest are skipped");
            break;
        }
        items.push(OfferedFile {
            id: items.len() as u32 + 1,
            path: path.clone(),
            name,
            size: metadata.len(),
        });
    }
    items
}

/// Replace the current offer (`items` empty withdraws it) and queue the
/// signaling message.
fn publish_offer(shared: &Mutex<Shared>, source: &'static str, items: Vec<OfferedFile>) {
    let mut state = lock(shared);
    if items.is_empty() && state.offer.is_none() {
        return;
    }
    let id = state.next_offer_id;
    state.next_offer_id = state.next_offer_id.wrapping_add(1).max(1);
    let offer = CurrentOffer { id, source, items };
    info!(
        offer_id = id,
        source,
        files = offer.items.len(),
        "file offer {}",
        if offer.items.is_empty() {
            "withdrawn"
        } else {
            "published"
        }
    );
    let message = offer.message();
    state.offer = (!offer.items.is_empty()).then_some(offer);
    if state.epoch.is_some() {
        state.outbox.signals.push(message);
    }
}

fn watch_loop(config: &FileServiceConfig, shared: &Mutex<Shared>, stop: &AtomicBool) {
    let mut last_seq: Option<u32> = None;
    while !stop.load(Ordering::Acquire) {
        sleep_unless_stopped(stop, WATCH_INTERVAL);
        let live = lock(shared).epoch.is_some();
        if !live {
            // Copies made while nobody watches are never offered.
            last_seq = None;
            continue;
        }
        if config.clipboard {
            poll_clipboard(shared, &mut last_seq);
        }
        if let Some(dir) = &config.sendto_dir {
            poll_spool(shared, dir);
        }
    }
}

fn poll_clipboard(shared: &Mutex<Shared>, last_seq: &mut Option<u32>) {
    let seq = crate::clipboard::sequence_number();
    let previous = last_seq.replace(seq);
    if seq == 0 || previous.is_none() || previous == Some(seq) {
        return;
    }
    if lock(shared).own_clipboard_seq == Some(seq) {
        return;
    }
    match crate::clipboard::read_files() {
        Ok(Some(paths)) => {
            let items = offer_items(&paths);
            publish_offer(shared, "clipboard", items);
        }
        Ok(None) => {
            // The clipboard moved on: withdraw a clipboard offer.
            let clipboard_offer = lock(shared)
                .offer
                .as_ref()
                .is_some_and(|offer| offer.source == "clipboard");
            if clipboard_offer {
                publish_offer(shared, "clipboard", Vec::new());
            }
        }
        Err(error) => debug!(?error, "clipboard file list unavailable"),
    }
}

fn poll_spool(shared: &Mutex<Shared>, dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut requests: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    if requests.is_empty() {
        return;
    }
    requests.sort();
    let mut newest: Option<SpoolRequest> = None;
    for path in requests {
        let parsed = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SpoolRequest>(&bytes).ok());
        let _ = fs::remove_file(&path);
        match parsed {
            Some(request)
                if request.protocol == 1
                    && unix_ms().saturating_sub(request.created_ms)
                        <= SPOOL_MAX_AGE.as_millis() as u64 =>
            {
                newest = Some(request)
            }
            _ => info!(path = %path.display(), "stale or invalid Send To request dropped"),
        }
    }
    if let Some(request) = newest {
        let items = offer_items(&request.paths);
        if items.is_empty() {
            info!("Send To request had no files to offer");
            return;
        }
        publish_offer(shared, "sendto", items);
    }
}

// ---------------------------------------------------------------------------
// Socket.

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

fn connect_file_socket(worker_url: &str, bearer: &str, epoch: &str) -> Result<Socket> {
    let request = host_socket_request(worker_url, bearer, epoch, HostSocket::File)?;
    let (mut ws, _) = tungstenite::connect(request).context("connect file socket")?;
    ws.send(Message::text(r#"{"type":"hello","role":"file-host"}"#))
        .context("send file hello")?;
    ws.flush().context("flush file hello")?;
    let tcp: &TcpStream = match ws.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::NativeTls(stream) => stream.get_ref(),
        other => bail!("unsupported TLS stream variant: {other:?}"),
    };
    tcp.set_nodelay(true).context("enable TCP_NODELAY")?;
    tcp.set_nonblocking(true).context("set_nonblocking")?;
    Ok(ws)
}

fn socket_loop(
    worker_url: &str,
    bearer: &str,
    config: &FileServiceConfig,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
) {
    let mut delay = RECONNECT_DELAY;
    while !stop.load(Ordering::Acquire) {
        let Some(epoch) = lock(shared).epoch.clone() else {
            sleep_unless_stopped(stop, Duration::from_millis(200));
            continue;
        };
        let mut ws = match connect_file_socket(worker_url, bearer, &epoch) {
            Ok(ws) => ws,
            Err(error) => {
                warn!(error = %format!("{error:#}"), ?delay, "file socket connect failed");
                sleep_unless_stopped(stop, delay);
                delay = (delay * 2).min(RECONNECT_DELAY_MAX);
                continue;
            }
        };
        delay = RECONNECT_DELAY;
        lock(shared).connected = true;
        debug!("file socket connected");
        let mut session = Session::new(config.clone());
        let result = session.serve(&mut ws, &epoch, shared, stop);
        session.abort_all("socket-closed");
        lock(shared).connected = false;
        let _ = ws.close(None);
        let _ = ws.flush();
        match result {
            Ok(()) => debug!("file socket closed"),
            Err(error) => debug!(error = %format!("{error:#}"), "file socket ended"),
        }
        sleep_unless_stopped(stop, RECONNECT_DELAY);
    }
}

/// An upload being received.
struct Receiving {
    state: Incoming,
    meta: OfferMeta,
    file: Option<File>,
    partial: PathBuf,
    hasher: Option<Sha256>,
    started: Instant,
}

/// Saved files of the batch being uploaded, placed on the clipboard together.
#[derive(Default)]
struct Batch {
    id: Option<u32>,
    saved: Vec<PathBuf>,
    purpose: Option<Purpose>,
}

enum SendPhase {
    AwaitAccept,
    Sending,
    AwaitResult,
}

/// A download being sent.
struct Sending {
    transfer_id: u32,
    name: String,
    file: File,
    size: u64,
    phase: SendPhase,
    window: SendWindow,
    hasher: Option<Sha256>,
    seq: u32,
    last_progress: Instant,
    started: Instant,
}

struct Session {
    config: FileServiceConfig,
    receiving: Option<Receiving>,
    batch: Batch,
    sending: Option<Sending>,
    requests: VecDeque<(u32, RequestPayload)>,
    limiter: RateLimiter,
    clock: Instant,
    send_wait_until: Option<Instant>,
    drop_upload: Option<DropUpload>,
}

struct DropUpload {
    id: u32,
    point: ft::DropPoint,
    offers: Vec<Option<(u32, OfferMeta)>>,
    receiving: HashMap<u32, Receiving>,
    batch: Option<DropBatch>,
    started: Instant,
}

impl Session {
    fn new(config: FileServiceConfig) -> Self {
        Self {
            config,
            receiving: None,
            batch: Batch::default(),
            sending: None,
            requests: VecDeque::new(),
            limiter: RateLimiter::new(),
            clock: Instant::now(),
            send_wait_until: None,
            drop_upload: None,
        }
    }

    fn serve(
        &mut self,
        ws: &mut Socket,
        epoch: &str,
        shared: &Mutex<Shared>,
        stop: &AtomicBool,
    ) -> Result<()> {
        let mut last_inbound = Instant::now();
        let mut last_outbound = Instant::now();
        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            if lock(shared).epoch.as_deref() != Some(epoch) {
                return Ok(());
            }
            let mut progressed = false;
            self.poll_drop(ws, shared)?;
            loop {
                match ws.read() {
                    Ok(Message::Binary(bytes)) => {
                        last_inbound = Instant::now();
                        progressed = true;
                        self.on_packet(ws, &bytes, shared)?;
                    }
                    Ok(Message::Text(text)) => {
                        last_inbound = Instant::now();
                        self.on_text(text.as_str());
                    }
                    Ok(Message::Close(_)) => bail!("file socket closed by relay"),
                    Ok(_) => last_inbound = Instant::now(),
                    Err(error) if would_block(&error) => break,
                    Err(error) => return Err(error).context("read file socket"),
                }
            }
            if let Some(receiving) = self.receiving.as_mut() {
                let now_ms = self.clock.elapsed().as_millis() as u64;
                if let Some(total) = receiving.state.ack_due(now_ms) {
                    let id = receiving.state.transfer_id;
                    send(
                        ws,
                        Packet {
                            transfer_id: id,
                            seq: 0,
                            body: Body::Ack(total),
                        },
                    )?;
                    last_outbound = Instant::now();
                }
            }
            if let Some(drop) = self.drop_upload.as_mut() {
                let now_ms = self.clock.elapsed().as_millis() as u64;
                for (&id, receiving) in &mut drop.receiving {
                    if let Some(total) = receiving.state.ack_due(now_ms) {
                        send(
                            ws,
                            Packet {
                                transfer_id: id,
                                seq: 0,
                                body: Body::Ack(total),
                            },
                        )?;
                        last_outbound = Instant::now();
                    }
                }
            }
            if self.sending.is_none() {
                self.start_next_request(ws, shared)?;
            }
            if self.pump_download(ws, shared)? {
                progressed = true;
                last_outbound = Instant::now();
            }
            match ws.flush() {
                Ok(()) => {}
                Err(error) if would_block(&error) => {}
                Err(error) => return Err(error).context("flush file socket"),
            }
            if last_outbound.elapsed() >= HEARTBEAT_INTERVAL {
                send_text(ws, r#"{"type":"ping"}"#)?;
                last_outbound = Instant::now();
            }
            if last_inbound.elapsed() >= INBOUND_SILENCE_LIMIT {
                bail!("file socket silent");
            }
            if !progressed {
                thread::sleep(Duration::from_millis(if self.busy() { 1 } else { 20 }));
            }
        }
    }

    fn busy(&self) -> bool {
        self.receiving.is_some() || self.sending.is_some() || self.drop_upload.is_some()
    }

    fn on_text(&mut self, text: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        if value["type"] == "file-peer" && value["present"] == true {
            // A (new) client: nothing carries over.
            self.abort_all("peer-replaced");
        }
    }

    fn on_packet(&mut self, ws: &mut Socket, bytes: &[u8], shared: &Mutex<Shared>) -> Result<()> {
        let packet = match ft::decode(bytes) {
            Ok(packet) => packet,
            Err(reject) => {
                warn!(reason = reject.code(), "file packet rejected");
                return Ok(());
            }
        };
        let id = packet.transfer_id;
        match packet.body {
            Body::Offer(meta) => self.on_offer(ws, id, meta, shared),
            Body::Data(data) => self.on_data(ws, id, packet.seq, data, shared),
            Body::Done(hash) => self.on_done(ws, id, &hash, shared),
            Body::Cancel(CancelPayload { reason }) => {
                if self.drop_upload.as_ref().is_some_and(|d| {
                    d.offers
                        .iter()
                        .flatten()
                        .any(|(offer_id, _)| *offer_id == id)
                }) {
                    self.cancel_drop(ws, "drop-cancelled", shared)?;
                }
                if self
                    .receiving
                    .as_ref()
                    .is_some_and(|r| r.state.transfer_id == id)
                {
                    info!(transfer = id, reason, "upload cancelled by the client");
                    self.fail_upload(None, id, "cancelled", shared);
                }
                if self.sending.as_ref().is_some_and(|s| s.transfer_id == id) {
                    info!(transfer = id, reason, "download cancelled by the client");
                    self.sending = None;
                    lock(shared).stats.downloads_failed += 1;
                }
                self.requests.retain(|(request_id, _)| *request_id != id);
                Ok(())
            }
            Body::Accept => {
                if let Some(sending) = self.sending.as_mut().filter(|s| s.transfer_id == id) {
                    if matches!(sending.phase, SendPhase::AwaitAccept) {
                        sending.phase = SendPhase::Sending;
                        sending.last_progress = Instant::now();
                    }
                }
                Ok(())
            }
            Body::Ack(total) => {
                let Some(sending) = self.sending.as_mut().filter(|s| s.transfer_id == id) else {
                    return Ok(());
                };
                if sending.window.on_ack(total).is_err() {
                    warn!(
                        transfer = id,
                        total, "download ACK out of range; cancelling"
                    );
                    self.cancel_download(ws, "unexpected", shared)?;
                } else {
                    sending.last_progress = Instant::now();
                }
                Ok(())
            }
            Body::Result(ResultPayload { ok, error }) => {
                let Some(sending) = self.sending.take_if(|s| s.transfer_id == id) else {
                    return Ok(());
                };
                let elapsed = sending.started.elapsed().as_secs_f64().max(0.001);
                let mut state = lock(shared);
                if ok {
                    state.stats.downloads_ok += 1;
                    info!(
                        transfer = id,
                        name = sending.name,
                        bytes = sending.size,
                        ms = (elapsed * 1000.0) as u64,
                        mbps = format!("{:.1}", sending.size as f64 * 8.0 / elapsed / 1e6),
                        "file download delivered"
                    );
                } else {
                    state.stats.downloads_failed += 1;
                    warn!(
                        transfer = id,
                        error = error.unwrap_or_default(),
                        "file download failed on the client"
                    );
                }
                Ok(())
            }
            Body::Request(request) => {
                if self.requests.len() >= MAX_QUEUED_REQUESTS {
                    return send_cancel(ws, id, "busy");
                }
                self.requests.push_back((id, request));
                Ok(())
            }
        }
    }

    // --- uploads (client -> host) -----------------------------------------

    fn on_drop_offer(
        &mut self,
        ws: &mut Socket,
        id: u32,
        meta: OfferMeta,
        shared: &Mutex<Shared>,
    ) -> Result<()> {
        if self.receiving.is_some() {
            return send_cancel(ws, id, "busy");
        }
        if self
            .drop_upload
            .as_ref()
            .is_some_and(|drop| drop.id != meta.batch_id)
        {
            return send_cancel(ws, id, "busy");
        }
        let point = meta.drop.expect("validated drop point");
        if self.drop_upload.is_none() {
            self.drop_upload = Some(DropUpload {
                id: meta.batch_id,
                point,
                offers: vec![None; meta.batch_count as usize],
                receiving: HashMap::new(),
                batch: None,
                started: Instant::now(),
            });
        }
        let drop = self.drop_upload.as_mut().unwrap();
        if drop.id != meta.batch_id
            || drop.point != point
            || drop.offers.len() != meta.batch_count as usize
            || drop.batch.is_some()
            || drop.offers[meta.batch_index as usize].is_some()
            || drop.offers.iter().flatten().any(|(prior, _)| *prior == id)
        {
            return send_cancel(ws, id, "invalid-drop");
        }
        let index = meta.batch_index as usize;
        drop.offers[index] = Some((id, meta));
        let total: u64 = drop.offers.iter().flatten().map(|(_, m)| m.size).sum();
        if total > ft::MAX_FILE_BYTES {
            return self.cancel_drop(ws, "drop-too-large", shared);
        }
        if drop.offers.iter().all(Option::is_some) {
            let metas = drop
                .offers
                .iter()
                .map(|o| o.as_ref().unwrap().1.clone())
                .collect();
            let batch = match DropBatch::new(metas) {
                Ok(batch) => batch,
                Err(error) => {
                    warn!(%error, "cannot create native drop cache");
                    return self.cancel_drop(ws, "write-failed", shared);
                }
            };
            drop.batch = Some(batch.clone());
            for (index, offer) in drop.offers.iter().enumerate() {
                let (id, meta) = offer.as_ref().unwrap();
                let path = batch.path(index);
                let prepare = (|| -> Result<Receiving> {
                    let file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)?;
                    write_mark_of_the_web(&path)?;
                    Ok(Receiving {
                        state: Incoming::new(*id, meta).map_err(|e| anyhow::anyhow!(e.code()))?,
                        meta: meta.clone(),
                        file: Some(file),
                        partial: path,
                        hasher: Some(Sha256::new()?),
                        started: Instant::now(),
                    })
                })();
                match prepare {
                    Ok(receiving) => {
                        drop.receiving.insert(*id, receiving);
                    }
                    Err(error) => {
                        warn!(%error, "cannot prepare native drop stream");
                        return self.cancel_drop(ws, "write-failed", shared);
                    }
                }
            }
            let mut state = lock(shared);
            let epoch = state.epoch.clone().unwrap_or_default();
            state
                .outbox
                .drops
                .push(NativeDropRequest::new(batch, point, epoch));
            info!(
                files = drop.offers.len(),
                bytes = total,
                "native drop metadata ready"
            );
        }
        send(
            ws,
            Packet {
                transfer_id: id,
                seq: 0,
                body: Body::Accept,
            },
        )
    }

    fn on_drop_data(
        &mut self,
        ws: &mut Socket,
        id: u32,
        seq: u32,
        data: &[u8],
        shared: &Mutex<Shared>,
    ) -> Result<()> {
        let drop = self.drop_upload.as_mut().unwrap();
        let receiving = drop.receiving.get_mut(&id).unwrap();
        if let Err(error) = receiving.state.on_data(seq, data.len()) {
            return self.cancel_drop(ws, error.code(), shared);
        }
        receiving.hasher.as_mut().unwrap().update(data);
        if let Err(error) = receiving.file.as_mut().unwrap().write_all(data) {
            warn!(%error, "native drop cache write failed");
            return self.cancel_drop(ws, "write-failed", shared);
        }
        drop.batch.as_ref().unwrap().progress();
        lock(shared).stats.bytes_in += data.len() as u64;
        let now_ms = self.clock.elapsed().as_millis() as u64;
        if let Some(total) = receiving.state.ack_due(now_ms) {
            send(
                ws,
                Packet {
                    transfer_id: id,
                    seq: 0,
                    body: Body::Ack(total),
                },
            )?;
        }
        Ok(())
    }

    fn on_drop_done(
        &mut self,
        ws: &mut Socket,
        id: u32,
        expected: &[u8; 32],
        shared: &Mutex<Shared>,
    ) -> Result<()> {
        let drop = self.drop_upload.as_mut().unwrap();
        let mut receiving = drop.receiving.remove(&id).unwrap();
        let actual = receiving.hasher.take().unwrap().finish();
        let file = receiving.file.take().unwrap();
        let flush_started = Instant::now();
        if let Err(error) = file.sync_all() {
            std::mem::drop(file);
            warn!(%error, "native drop cache flush failed");
            return self.cancel_drop(ws, "write-failed", shared);
        }
        std::mem::drop(file);
        if let Err(error) = receiving.state.on_done(expected, &actual) {
            return self.cancel_drop(ws, error.code(), shared);
        }
        drop.batch
            .as_ref()
            .unwrap()
            .verified(receiving.meta.batch_index as usize);
        info!(
            index = receiving.meta.batch_index,
            bytes = receiving.meta.size,
            elapsed_ms = receiving.started.elapsed().as_millis() as u64,
            flush_ms = flush_started.elapsed().as_millis() as u64,
            "native drop spool verified"
        );
        // DONE verifies this spool, not target delivery. RESULT is deferred.
        send(
            ws,
            Packet {
                transfer_id: id,
                seq: 0,
                body: Body::Ack(receiving.meta.size),
            },
        )?;
        self.poll_drop(ws, shared)
    }

    fn poll_drop(&mut self, ws: &mut Socket, shared: &Mutex<Shared>) -> Result<()> {
        let Some(drop) = &self.drop_upload else {
            return Ok(());
        };
        let result = if let Some(batch) = &drop.batch {
            batch.result()
        } else if drop.started.elapsed() > Duration::from_secs(120) {
            Some(Err("drop-timeout".into()))
        } else {
            None
        };
        let Some(result) = result else {
            return Ok(());
        };
        if let Err(reason) = result {
            return self.cancel_drop(ws, &reason, shared);
        }
        let drop = self.drop_upload.take().unwrap();
        lock(shared).stats.uploads_ok += drop.offers.len() as u64;
        for (id, _) in drop.offers.into_iter().flatten() {
            send(
                ws,
                Packet {
                    transfer_id: id,
                    seq: 0,
                    body: Body::Result(ResultPayload {
                        ok: true,
                        error: None,
                    }),
                },
            )?;
        }
        info!("native file drop accepted with verified contents");
        Ok(())
    }

    fn cancel_drop(&mut self, ws: &mut Socket, reason: &str, shared: &Mutex<Shared>) -> Result<()> {
        let Some(mut drop) = self.drop_upload.take() else {
            return Ok(());
        };
        if let Some(batch) = &drop.batch {
            batch.cancel(reason);
        }
        drop.receiving.clear(); // close writes before last cache owner removes it
        let count = drop.offers.iter().flatten().count();
        lock(shared).stats.uploads_failed += count as u64;
        for (id, _) in drop.offers.into_iter().flatten() {
            send_cancel(ws, id, reason)?;
        }
        info!(reason, "native file drop cancelled");
        Ok(())
    }

    fn on_offer(
        &mut self,
        ws: &mut Socket,
        id: u32,
        meta: OfferMeta,
        shared: &Mutex<Shared>,
    ) -> Result<()> {
        if meta.purpose == Purpose::Drop {
            return self.on_drop_offer(ws, id, meta, shared);
        }
        if self.drop_upload.is_some() {
            return send_cancel(ws, id, "busy");
        }
        if self.receiving.is_some() {
            return send_cancel(ws, id, "busy");
        }
        if self.batch.id != Some(meta.batch_id) {
            self.batch = Batch {
                id: Some(meta.batch_id),
                saved: Vec::new(),
                purpose: Some(meta.purpose),
            };
        }
        let state = match Incoming::new(id, &meta) {
            Ok(state) => state,
            Err(error) => return send_cancel(ws, id, error.code()),
        };
        let (file, partial) = match create_partial(&self.config.downloads_dir, id, &meta.name) {
            Ok(created) => created,
            Err(error) => {
                warn!(error = %format!("{error:#}"), "cannot create upload file");
                return send_cancel(ws, id, "write-failed");
            }
        };
        info!(transfer = id, name = meta.name, size = meta.size, purpose = ?meta.purpose, "file upload started");
        self.receiving = Some(Receiving {
            state,
            meta,
            file: Some(file),
            partial,
            hasher: Some(Sha256::new()?),
            started: Instant::now(),
        });
        send(
            ws,
            Packet {
                transfer_id: id,
                seq: 0,
                body: Body::Accept,
            },
        )
    }

    fn on_data(
        &mut self,
        ws: &mut Socket,
        id: u32,
        seq: u32,
        data: &[u8],
        shared: &Mutex<Shared>,
    ) -> Result<()> {
        if self
            .drop_upload
            .as_ref()
            .is_some_and(|d| d.receiving.contains_key(&id))
        {
            return self.on_drop_data(ws, id, seq, data, shared);
        }
        let Some(receiving) = self
            .receiving
            .as_mut()
            .filter(|r| r.state.transfer_id == id)
        else {
            return Ok(());
        };
        if let Err(error) = receiving.state.on_data(seq, data.len()) {
            self.fail_upload(Some(ws), id, error.code(), shared);
            return Ok(());
        }
        receiving
            .hasher
            .as_mut()
            .expect("hasher until done")
            .update(data);
        let write = receiving
            .file
            .as_mut()
            .expect("file until done")
            .write_all(data);
        if let Err(error) = write {
            warn!(?error, "upload write failed");
            self.fail_upload(Some(ws), id, "write-failed", shared);
            return Ok(());
        }
        lock(shared).stats.bytes_in += data.len() as u64;
        Ok(())
    }

    fn on_done(
        &mut self,
        ws: &mut Socket,
        id: u32,
        expected: &[u8; 32],
        shared: &Mutex<Shared>,
    ) -> Result<()> {
        if self
            .drop_upload
            .as_ref()
            .is_some_and(|d| d.receiving.contains_key(&id))
        {
            return self.on_drop_done(ws, id, expected, shared);
        }
        let Some(mut receiving) = self.receiving.take_if(|r| r.state.transfer_id == id) else {
            return Ok(());
        };
        let actual = receiving.hasher.take().expect("hasher until done").finish();
        let flushed = receiving
            .file
            .take()
            .expect("file until done")
            .sync_all()
            .is_ok();
        let verdict = receiving.state.on_done(expected, &actual);
        let result = match (verdict, flushed) {
            (Err(error), _) => Err(error.code()),
            (Ok(()), false) => Err("write-failed"),
            (Ok(()), true) => {
                finalize_upload(&self.config.downloads_dir, &receiving).map_err(|error| {
                    warn!(error = %format!("{error:#}"), "cannot finalize upload");
                    "write-failed"
                })
            }
        };
        match result {
            Ok(path) => {
                let elapsed = receiving.started.elapsed().as_secs_f64().max(0.001);
                info!(
                    transfer = id,
                    path = %path.display(),
                    bytes = receiving.meta.size,
                    ms = (elapsed * 1000.0) as u64,
                    mbps = format!("{:.1}", receiving.meta.size as f64 * 8.0 / elapsed / 1e6),
                    "file upload saved"
                );
                lock(shared).stats.uploads_ok += 1;
                send(
                    ws,
                    Packet {
                        transfer_id: id,
                        seq: 0,
                        body: Body::Result(ResultPayload {
                            ok: true,
                            error: None,
                        }),
                    },
                )?;
                self.batch.saved.push(path);
            }
            Err(code) => {
                let _ = fs::remove_file(&receiving.partial);
                lock(shared).stats.uploads_failed += 1;
                warn!(
                    transfer = id,
                    error = code,
                    "file upload rejected; partial file removed"
                );
                send(
                    ws,
                    Packet {
                        transfer_id: id,
                        seq: 0,
                        body: Body::Result(ResultPayload {
                            ok: false,
                            error: Some(code.to_string()),
                        }),
                    },
                )?;
            }
        }
        if receiving.meta.batch_index + 1 == receiving.meta.batch_count {
            self.finish_batch(shared);
        }
        Ok(())
    }

    /// Remove the partial file and tell the sender (CANCEL) when `ws` is given.
    fn fail_upload(
        &mut self,
        ws: Option<&mut Socket>,
        id: u32,
        code: &str,
        shared: &Mutex<Shared>,
    ) {
        let Some(mut receiving) = self.receiving.take() else {
            return;
        };
        drop(receiving.file.take());
        if let Err(error) = fs::remove_file(&receiving.partial) {
            warn!(?error, path = %receiving.partial.display(), "cannot remove partial upload");
        }
        lock(shared).stats.uploads_failed += 1;
        info!(
            transfer = id,
            error = code,
            "file upload failed; partial file removed"
        );
        if let Some(ws) = ws {
            let _ = send_cancel(ws, id, code);
        }
        // The files of the batch that did arrive still reach the clipboard.
        if receiving.meta.batch_index + 1 == receiving.meta.batch_count {
            self.finish_batch(shared);
        }
    }

    /// The last file of a batch is done: put the saved files on the
    /// clipboard, and ask for a paste when the batch was a paste.
    fn finish_batch(&mut self, shared: &Mutex<Shared>) {
        let batch = std::mem::take(&mut self.batch);
        if batch.saved.is_empty() {
            return;
        }
        if !self.config.clipboard {
            info!(
                files = batch.saved.len(),
                "clipboard placement skipped (BUBBLEPLAY_FILE_CLIPBOARD=0)"
            );
            return;
        }
        let mut state = lock(shared);
        match crate::clipboard::set_files(&batch.saved) {
            Ok(seq) => {
                state.own_clipboard_seq = Some(seq);
                info!(
                    files = batch.saved.len(),
                    "uploaded files placed on the clipboard"
                );
                if batch.purpose == Some(Purpose::Paste) {
                    state.outbox.pastes += 1;
                }
            }
            Err(error) => {
                warn!(error = %format!("{error:#}"), "cannot place uploaded files on the clipboard")
            }
        }
    }

    // --- downloads (host -> client) ---------------------------------------

    fn start_next_request(&mut self, ws: &mut Socket, shared: &Mutex<Shared>) -> Result<()> {
        while let Some((id, request)) = self.requests.pop_front() {
            let offered = {
                let state = lock(shared);
                state.offer.as_ref().and_then(|offer| {
                    (offer.id == request.offer_id)
                        .then(|| {
                            let index =
                                offer.items.iter().position(|item| item.id == request.id)?;
                            Some((
                                offer.items[index].clone(),
                                index as u32,
                                offer.items.len() as u32,
                            ))
                        })
                        .flatten()
                })
            };
            let Some((item, index, count)) = offered else {
                send_cancel(ws, id, "unknown-offer")?;
                continue;
            };
            let opened = File::open(&item.path).and_then(|file| {
                let metadata = file.metadata()?;
                Ok((file, metadata))
            });
            let (file, metadata) = match opened {
                Ok(opened) => opened,
                Err(error) => {
                    warn!(?error, path = %item.path.display(), "offered file cannot be read");
                    send_cancel(ws, id, "unreadable")?;
                    continue;
                }
            };
            if !metadata.is_file() || metadata.len() > ft::MAX_FILE_BYTES {
                send_cancel(ws, id, "too-large")?;
                continue;
            }
            let mtime_ms = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_millis() as u64);
            let meta = OfferMeta {
                name: item.name.clone(),
                size: metadata.len(),
                mime: mime_for(&item.name).to_string(),
                mtime_ms,
                purpose: Purpose::Save,
                batch_id: request.offer_id,
                batch_index: index,
                batch_count: count,
                drop: None,
            };
            info!(
                transfer = id,
                name = item.name,
                size = meta.size,
                "file download requested"
            );
            send(
                ws,
                Packet {
                    transfer_id: id,
                    seq: 0,
                    body: Body::Offer(meta.clone()),
                },
            )?;
            self.sending = Some(Sending {
                transfer_id: id,
                name: item.name,
                file,
                size: meta.size,
                phase: SendPhase::AwaitAccept,
                window: SendWindow::default(),
                hasher: Some(Sha256::new()?),
                seq: 0,
                last_progress: Instant::now(),
                started: Instant::now(),
            });
            return Ok(());
        }
        Ok(())
    }

    /// Send as much DATA as the window, the socket and the rate cap allow.
    /// Returns true when anything was sent.
    fn pump_download(&mut self, ws: &mut Socket, shared: &Mutex<Shared>) -> Result<bool> {
        let Some(sending) = self.sending.as_mut() else {
            return Ok(false);
        };
        if !matches!(sending.phase, SendPhase::Sending) {
            if sending.last_progress.elapsed() >= ACK_STALL_LIMIT {
                warn!(
                    transfer = sending.transfer_id,
                    "download stalled waiting for the client"
                );
                self.cancel_download(ws, "timeout", shared)?;
            }
            return Ok(false);
        }
        if sending.window.sent() < sending.size
            && sending.window.sent() > sending.window.acked()
            && sending.last_progress.elapsed() >= ACK_STALL_LIMIT
        {
            warn!(transfer = sending.transfer_id, "download ACKs stalled");
            self.cancel_download(ws, "timeout", shared)?;
            return Ok(false);
        }
        if self
            .send_wait_until
            .is_some_and(|until| Instant::now() < until)
        {
            return Ok(false);
        }
        self.send_wait_until = None;
        let mut sent_any = false;
        let mut chunk = vec![0u8; ft::MAX_DATA_BYTES];
        // A few chunks per loop turn keep ACKs and uploads moving.
        for _ in 0..8 {
            let sending = self.sending.as_mut().expect("checked above");
            let remaining = sending.size - sending.window.sent();
            if remaining == 0 {
                let hash = sending.hasher.take().expect("hasher until done").finish();
                let id = sending.transfer_id;
                sending.phase = SendPhase::AwaitResult;
                sending.last_progress = Instant::now();
                send(
                    ws,
                    Packet {
                        transfer_id: id,
                        seq: 0,
                        body: Body::Done(hash),
                    },
                )?;
                return Ok(true);
            }
            let len = remaining.min(ft::MAX_DATA_BYTES as u64) as usize;
            if !sending.window.can_send(len) {
                break;
            }
            // Never let the socket buffer grow past one window either.
            match ws.flush() {
                Ok(()) => {}
                Err(error) if would_block(&error) => break,
                Err(error) => return Err(error).context("flush file socket"),
            }
            let cap = lock(shared).rate_cap_bps;
            let now_ms = self.clock.elapsed().as_millis() as u64;
            let wait = self.limiter.wait_ms(now_ms, cap, len);
            if wait > 0 {
                self.send_wait_until = Some(Instant::now() + Duration::from_millis(wait));
                break;
            }
            let sending = self.sending.as_mut().expect("checked above");
            let read = read_exact_or_eof(&mut sending.file, &mut chunk[..len]);
            match read {
                Ok(n) if n == len => {}
                Ok(_) => {
                    warn!(
                        transfer = sending.transfer_id,
                        "offered file shrank while sending"
                    );
                    self.cancel_download(ws, TransferError::SizeMismatch.code(), shared)?;
                    return Ok(sent_any);
                }
                Err(error) => {
                    warn!(?error, "offered file read failed");
                    self.cancel_download(ws, "unreadable", shared)?;
                    return Ok(sent_any);
                }
            }
            sending
                .hasher
                .as_mut()
                .expect("hasher until done")
                .update(&chunk[..len]);
            let (id, seq) = (sending.transfer_id, sending.seq);
            sending.seq = sending.seq.wrapping_add(1);
            sending.window.on_sent(len);
            write(
                ws,
                Packet {
                    transfer_id: id,
                    seq,
                    body: Body::Data(&chunk[..len]),
                },
            )?;
            lock(shared).stats.bytes_out += len as u64;
            sent_any = true;
        }
        Ok(sent_any)
    }

    fn cancel_download(
        &mut self,
        ws: &mut Socket,
        reason: &str,
        shared: &Mutex<Shared>,
    ) -> Result<()> {
        if let Some(sending) = self.sending.take() {
            lock(shared).stats.downloads_failed += 1;
            send_cancel(ws, sending.transfer_id, reason)?;
        }
        Ok(())
    }

    /// Socket gone or a new peer: drop everything, delete partial uploads.
    fn abort_all(&mut self, reason: &str) {
        if let Some(mut drop) = self.drop_upload.take() {
            if let Some(batch) = &drop.batch {
                batch.cancel(reason);
            }
            drop.receiving.clear();
        }
        if let Some(mut receiving) = self.receiving.take() {
            drop(receiving.file.take());
            let _ = fs::remove_file(&receiving.partial);
            info!(
                transfer = receiving.state.transfer_id,
                reason, "upload aborted; partial file removed"
            );
        }
        if let Some(sending) = self.sending.take() {
            info!(transfer = sending.transfer_id, reason, "download aborted");
        }
        self.requests.clear();
        self.batch = Batch::default();
    }
}

fn read_exact_or_eof(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn would_block(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(io) if io.kind() == ErrorKind::WouldBlock)
}

/// Queue a packet without waiting for the socket to drain.
fn write(ws: &mut Socket, packet: Packet<'_>) -> Result<()> {
    match ws.write(Message::binary(ft::encode(&packet))) {
        Ok(()) => Ok(()),
        Err(error) if would_block(&error) => Ok(()),
        Err(error) => Err(error).context("write file socket"),
    }
}

fn send(ws: &mut Socket, packet: Packet<'_>) -> Result<()> {
    write(ws, packet)?;
    match ws.flush() {
        Ok(()) => Ok(()),
        Err(error) if would_block(&error) => Ok(()),
        Err(error) => Err(error).context("flush file socket"),
    }
}

fn send_text(ws: &mut Socket, text: &str) -> Result<()> {
    match ws.write(Message::text(text)) {
        Ok(()) => Ok(()),
        Err(error) if would_block(&error) => Ok(()),
        Err(error) => Err(error).context("write file socket"),
    }
}

fn send_cancel(ws: &mut Socket, id: u32, reason: &str) -> Result<()> {
    send(
        ws,
        Packet {
            transfer_id: id,
            seq: 0,
            body: Body::Cancel(CancelPayload {
                reason: reason.to_string(),
            }),
        },
    )
}

// ---------------------------------------------------------------------------
// Files on disk.

const MOTW: &[u8] = b"[ZoneTransfer]\r\nZoneId=3\r\n";

/// Write the Mark-of-the-Web (Internet zone) on `path`.
pub fn write_mark_of_the_web(path: &Path) -> Result<()> {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    fs::write(PathBuf::from(stream), MOTW).context("write Zone.Identifier")
}

/// Create the partial file (with its Mark-of-the-Web) for an upload.
fn create_partial(dir: &Path, id: u32, name: &str) -> Result<(File, PathBuf)> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let safe = ft::sanitize_file_name(name);
    let partial = dir.join(format!(".{safe}.{id:08x}{PARTIAL_SUFFIX}"));
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&partial)
        .with_context(|| format!("create {}", partial.display()))?;
    if let Err(error) = write_mark_of_the_web(&partial) {
        drop(file);
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    Ok((file, partial))
}

/// Move a verified upload to its final, unique name.
fn finalize_upload(dir: &Path, receiving: &Receiving) -> Result<PathBuf> {
    let target = reserve_unique_name(dir, &ft::sanitize_file_name(&receiving.meta.name))?;
    if let Err(error) = fs::rename(&receiving.partial, &target) {
        let _ = fs::remove_file(&target);
        return Err(error).context("rename upload into place");
    }
    // The stream follows the file on rename; write it again in case the
    // file system dropped it.
    if fs::metadata(format!("{}:Zone.Identifier", target.display())).is_err() {
        write_mark_of_the_web(&target)?;
    }
    Ok(target)
}

/// Claim `name`, or `name (n)` for the first free n, by creating it.
pub fn reserve_unique_name(dir: &Path, name: &str) -> Result<PathBuf> {
    for n in 0..10_000u32 {
        let candidate = if n == 0 {
            dir.join(name)
        } else {
            dir.join(ft::collision_name(name, n))
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", candidate.display()))
            }
        }
    }
    bail!("no free name for {name}")
}

fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" | "log" => "text/plain",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "json" => "application/json",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bubbleplay-file-test-{name}-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta(name: &str, size: u64) -> OfferMeta {
        OfferMeta {
            name: name.into(),
            size,
            mime: String::new(),
            mtime_ms: 0,
            purpose: Purpose::Save,
            batch_id: 1,
            batch_index: 0,
            batch_count: 1,
            drop: None,
        }
    }

    fn receiving(dir: &Path, name: &str, size: u64) -> Receiving {
        let meta = meta(name, size);
        let (file, partial) = create_partial(dir, 7, name).unwrap();
        Receiving {
            state: Incoming::new(7, &meta).unwrap(),
            meta,
            file: Some(file),
            partial,
            hasher: Some(Sha256::new().unwrap()),
            started: Instant::now(),
        }
    }

    fn native_test_session() -> (
        Session,
        Socket,
        WebSocket<TcpStream>,
        Mutex<Shared>,
        PathBuf,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let ws = WebSocket::from_raw_socket(
            MaybeTlsStream::Plain(server),
            tungstenite::protocol::Role::Server,
            None,
        );
        let client = WebSocket::from_raw_socket(client, tungstenite::protocol::Role::Client, None);
        static NEXT_NATIVE_TEST: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(1);
        let dir = scratch(&format!(
            "native-socket-{}",
            NEXT_NATIVE_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        let session = Session::new(FileServiceConfig {
            downloads_dir: dir.clone(),
            sendto_dir: None,
            clipboard: false,
        });
        let shared = Mutex::new(Shared {
            epoch: Some("test".into()),
            rate_cap_bps: 0,
            offer: None,
            next_offer_id: 1,
            outbox: FileOutbox::default(),
            own_clipboard_seq: None,
            stats: FileStats::default(),
            connected: true,
        });
        (session, ws, client, shared, dir)
    }
    fn drop_meta(index: u32, count: u32) -> OfferMeta {
        OfferMeta {
            purpose: Purpose::Drop,
            batch_index: index,
            batch_count: count,
            drop: Some(ft::DropPoint {
                x: 12,
                y: 34,
                generation: 1,
                viewport_revision: 2,
            }),
            ..meta("synthetic.txt", 3)
        }
    }
    fn read_packet(client: &mut WebSocket<TcpStream>) -> Vec<u8> {
        match client.read().unwrap() {
            Message::Binary(bytes) => bytes.to_vec(),
            other => panic!("unexpected {other:?}"),
        }
    }
    fn read_non_ack(client: &mut WebSocket<TcpStream>) -> Vec<u8> {
        loop {
            let bytes = read_packet(client);
            if !matches!(ft::decode(&bytes).unwrap().body, Body::Ack(_)) {
                return bytes;
            }
        }
    }
    #[test]
    fn prepared_large_upload_rejects_second_batch_and_bad_hash_without_delivery() {
        let (mut session, mut ws, mut client, shared, dir) = native_test_session();
        let mut large = drop_meta(0, 1);
        large.size = crate::native_file_drop::PREPARE_NATIVE_DROP_BYTES;
        session
            .on_offer(&mut ws, 41, large.clone(), &shared)
            .unwrap();
        read_packet(&mut client);
        let path = session
            .drop_upload
            .as_ref()
            .unwrap()
            .batch
            .as_ref()
            .unwrap()
            .path(0);
        let request = lock(&shared).outbox.drops.pop().unwrap();
        assert!(request.preparation_required());
        assert_eq!(request.preparation_status(), Ok(false));
        let mut next = drop_meta(0, 1);
        next.batch_id += 1;
        session.on_offer(&mut ws, 42, next, &shared).unwrap();
        match ft::decode(&read_packet(&mut client)).unwrap().body {
            Body::Cancel(payload) => assert_eq!(payload.reason, "busy"),
            other => panic!("unexpected {other:?}"),
        }
        let chunk = vec![0; ft::MAX_DATA_BYTES];
        for seq in 0..(large.size / chunk.len() as u64) {
            session
                .on_data(&mut ws, 41, seq as u32, &chunk, &shared)
                .unwrap();
        }
        assert_eq!(
            request.preparation_status(),
            Ok(false),
            "bytes alone never make the batch deliverable"
        );
        session.on_done(&mut ws, 41, &[0; 32], &shared).unwrap();
        assert_eq!(request.preparation_status(), Err("hash-mismatch".into()));
        match ft::decode(&read_non_ack(&mut client)).unwrap().body {
            Body::Cancel(payload) => assert_eq!(payload.reason, "hash-mismatch"),
            other => panic!("unexpected {other:?}"),
        }
        request.reject("test-cleanup");
        assert!(session.drop_upload.is_none());
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn native_batch_starts_at_metadata_and_results_wait_for_every_hash() {
        let (mut session, mut ws, mut client, shared, dir) = native_test_session();
        session
            .on_offer(&mut ws, 1, drop_meta(0, 2), &shared)
            .unwrap();
        assert!(matches!(
            ft::decode(&read_packet(&mut client)).unwrap().body,
            Body::Accept
        ));
        assert!(lock(&shared).outbox.drops.is_empty());
        session
            .on_offer(&mut ws, 2, drop_meta(1, 2), &shared)
            .unwrap();
        read_packet(&mut client);
        assert_eq!(lock(&shared).outbox.drops.len(), 1);
        // A fake target accepts; hashes still gate RESULT for the entire batch.
        session
            .drop_upload
            .as_ref()
            .unwrap()
            .batch
            .as_ref()
            .unwrap()
            .accepted_for_test();
        let mut hash = Sha256::new().unwrap();
        hash.update(b"abc");
        let digest = hash.finish();
        session.on_data(&mut ws, 1, 0, b"abc", &shared).unwrap();
        session.on_done(&mut ws, 1, &digest, &shared).unwrap();
        assert!(matches!(
            ft::decode(&read_packet(&mut client)).unwrap().body,
            Body::Ack(3)
        ));
        assert_eq!(lock(&shared).stats.uploads_ok, 0);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "drop files never reach Downloads"
        );
        session.on_data(&mut ws, 2, 0, b"abc", &shared).unwrap();
        session.on_done(&mut ws, 2, &digest, &shared).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                ft::decode(&read_non_ack(&mut client)).unwrap().body,
                Body::Result(ResultPayload { ok: true, .. })
            ));
        }
        assert_eq!(lock(&shared).stats.uploads_ok, 2);
        assert_eq!(lock(&shared).outbox.pastes, 0);
        lock(&shared).outbox.drops.clear();
        fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn native_target_reject_and_hash_failure_cancel_and_remove_private_cache() {
        for reject_target in [true, false] {
            let (mut session, mut ws, mut client, shared, dir) = native_test_session();
            session
                .on_offer(&mut ws, 7, drop_meta(0, 1), &shared)
                .unwrap();
            read_packet(&mut client);
            let path = session
                .drop_upload
                .as_ref()
                .unwrap()
                .batch
                .as_ref()
                .unwrap()
                .path(0);
            let request = lock(&shared).outbox.drops.pop().unwrap();
            if reject_target {
                request.reject("drop-unavailable");
                session.poll_drop(&mut ws, &shared).unwrap();
            } else {
                session.on_data(&mut ws, 7, 0, b"abc", &shared).unwrap();
                session.on_done(&mut ws, 7, &[0; 32], &shared).unwrap();
                request.reject("test-cleanup");
            }
            let bytes = read_non_ack(&mut client);
            match ft::decode(&bytes).unwrap().body {
                Body::Cancel(payload) => assert_eq!(
                    payload.reason,
                    if reject_target {
                        "drop-unavailable"
                    } else {
                        "hash-mismatch"
                    }
                ),
                other => panic!("unexpected {other:?}"),
            }
            assert!(session.drop_upload.is_none());
            assert!(!path.exists());
            assert_eq!(lock(&shared).stats.uploads_failed, 1);
            fs::remove_dir(&dir).unwrap();
        }
    }

    #[test]
    fn partial_and_final_files_carry_the_mark_of_the_web() {
        let dir = scratch("motw");
        let mut upload = receiving(&dir, "report.pdf", 5);
        upload.file.as_mut().unwrap().write_all(b"hello").unwrap();
        drop(upload.file.take());
        let motw = fs::read(format!("{}:Zone.Identifier", upload.partial.display())).unwrap();
        assert_eq!(motw, MOTW);
        let target = finalize_upload(&dir, &upload).unwrap();
        assert_eq!(target, dir.join("report.pdf"));
        assert_eq!(fs::read(&target).unwrap(), b"hello");
        let motw = fs::read_to_string(format!("{}:Zone.Identifier", target.display())).unwrap();
        assert!(motw.contains("ZoneId=3"), "{motw}");
        assert!(!upload.partial.exists());
        // A second upload of the same name gets " (1)".
        let mut again = receiving(&dir, "report.pdf", 1);
        again.file.as_mut().unwrap().write_all(b"x").unwrap();
        drop(again.file.take());
        assert_eq!(
            finalize_upload(&dir, &again).unwrap(),
            dir.join("report (1).pdf")
        );
        // Unsafe names never escape the directory.
        let mut evil = receiving(&dir, "..\\..\\evil.exe", 1);
        drop(evil.file.take());
        assert_eq!(finalize_upload(&dir, &evil).unwrap(), dir.join("evil.exe"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_or_cancelled_uploads_leave_no_partial_file() {
        let dir = scratch("partial");
        let shared = Mutex::new(Shared {
            epoch: None,
            rate_cap_bps: 0,
            offer: None,
            next_offer_id: 1,
            outbox: FileOutbox::default(),
            own_clipboard_seq: None,
            stats: FileStats::default(),
            connected: false,
        });
        let config = FileServiceConfig {
            downloads_dir: dir.clone(),
            sendto_dir: None,
            clipboard: false,
        };
        // Cancel mid-file.
        let mut session = Session::new(config.clone());
        session.receiving = Some(receiving(&dir, "a.bin", 10));
        let partial = session.receiving.as_ref().unwrap().partial.clone();
        assert!(partial.exists());
        session.fail_upload(None, 7, "cancelled", &shared);
        assert!(!partial.exists());
        // Socket drop.
        session.receiving = Some(receiving(&dir, "b.bin", 10));
        let partial = session.receiving.as_ref().unwrap().partial.clone();
        session.abort_all("socket-closed");
        assert!(!partial.exists());
        assert_eq!(lock(&shared).stats.uploads_failed, 1);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spool_requests_become_file_offers_and_replace_each_other() {
        let dir = scratch("spool");
        let files = scratch("spool-files");
        let a = files.join("a.txt");
        let b = files.join("b.bin");
        fs::write(&a, b"aaa").unwrap();
        fs::write(&b, b"bb").unwrap();
        let sub = files.join("folder");
        fs::create_dir_all(&sub).unwrap();
        let shared = Mutex::new(Shared {
            epoch: Some("e".into()),
            rate_cap_bps: 0,
            offer: None,
            next_offer_id: 1,
            outbox: FileOutbox::default(),
            own_clipboard_seq: None,
            stats: FileStats::default(),
            connected: false,
        });
        write_spool_request(&dir, &[a.clone(), sub.clone(), b.clone()]).unwrap();
        poll_spool(&shared, &dir);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "spool files are consumed"
        );
        let first: serde_json::Value =
            serde_json::from_str(&lock(&shared).outbox.signals[0]).unwrap();
        assert_eq!(first["source"], "sendto");
        assert_eq!(first["offerId"], 1);
        assert_eq!(
            first["items"].as_array().unwrap().len(),
            2,
            "folders are skipped"
        );
        assert_eq!(first["items"][0]["name"], "a.txt");
        assert_eq!(first["items"][0]["size"], 3);
        // A new request replaces the offer; an empty one withdraws it.
        write_spool_request(&dir, std::slice::from_ref(&b)).unwrap();
        poll_spool(&shared, &dir);
        publish_offer(&shared, "clipboard", Vec::new());
        let state = lock(&shared);
        let messages: Vec<serde_json::Value> = state
            .outbox
            .signals
            .iter()
            .map(|text| serde_json::from_str(text).unwrap())
            .collect();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["offerId"], 2);
        assert_eq!(messages[1]["items"].as_array().unwrap().len(), 1);
        assert_eq!(messages[2]["items"].as_array().unwrap().len(), 0);
        assert!(state.offer.is_none());
        drop(state);
        // Stale requests are dropped without an offer.
        fs::write(
            dir.join("1-1.json"),
            serde_json::to_vec(&SpoolRequest {
                protocol: 1,
                created_ms: 1,
                paths: vec![a.clone()],
            })
            .unwrap(),
        )
        .unwrap();
        poll_spool(&shared, &dir);
        assert_eq!(lock(&shared).outbox.signals.len(), 3);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&files);
    }
}
