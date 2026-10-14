//! Bounded, ordered decode on its own MTA thread. Network ACKs are independent
//! of this worker; no encoded backlog is allowed to grow while an MFT stalls.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

use super::order::{ChainAction, InputTag, OrderTracker};
use crate::{
    media::{Codec, DecodeBackend, DecodedFrame, MediaRuntime, MfBackend},
    net::{DecoderFeedback, NetEvent, VideoFrame},
};

const MAX_QUEUED: usize = 4;
const STALL_LIMIT: Duration = Duration::from_millis(750);

#[derive(Clone, Copy, Debug)]
pub struct FrameMeta {
    pub local_epoch: u64,
    pub generation: u32,
    pub seq: u32,
    pub received_at: Instant,
    pub pts_100ns: i64,
}

pub struct DecodeCallbacks<P, R, E, D> {
    pub publish: P,
    pub reset: R,
    pub error: E,
    pub dropped: D,
}

/// Callbacks connect the transport-independent media mailbox and stats. They
/// run on this thread and must not block on the UI.
pub fn run<P, R, E, D>(
    device: ID3D11Device,
    events: Receiver<NetEvent>,
    feedback: Sender<DecoderFeedback>,
    stop: Arc<AtomicBool>,
    mut callbacks: DecodeCallbacks<P, R, E, D>,
) -> Result<()>
where
    P: FnMut(DecodedFrame, FrameMeta, Instant),
    R: FnMut(u64, &str, Instant),
    E: FnMut(String),
    D: FnMut(u64),
{
    let _runtime = MediaRuntime::new().context("decode thread Media Foundation startup")?;
    let mut backend: Option<MfBackend> = None;
    let mut order = OrderTracker::default();
    let mut pending: VecDeque<(VideoFrame, FrameMeta)> = VecDeque::new();
    let mut submitted: BTreeMap<i64, FrameMeta> = BTreeMap::new();
    let mut ordinal = 0i64;
    let mut epoch = 0;
    while !stop.load(Ordering::Acquire) {
        let wait = if pending.is_empty() && submitted.is_empty() {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(1)
        };
        let event = match events.recv_timeout(wait) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(event) = event {
            match event {
                NetEvent::Reset(reset) => {
                    epoch = epoch.max(reset.local_epoch);
                    order.reset(epoch);
                    (callbacks.dropped)((pending.len() + submitted.len()) as u64);
                    pending.clear();
                    submitted.clear();
                    backend = None;
                    (callbacks.reset)(epoch, &reset.reason, Instant::now());
                }
                NetEvent::Frame(frame) => {
                    let tag = InputTag {
                        epoch: frame.local_epoch,
                        generation: frame.generation,
                        sequence: frame.seq,
                        codec: frame.codec.packet_code(),
                        width: frame.width,
                        height: frame.height,
                    };
                    let action = order.accept(tag);
                    if action == ChainAction::Stale {
                        (callbacks.dropped)(1);
                        continue;
                    }
                    epoch = frame.local_epoch;
                    if action == ChainAction::Recreate {
                        (callbacks.dropped)((pending.len() + submitted.len()) as u64);
                        pending.clear();
                        submitted.clear();
                        backend = None;
                        (callbacks.reset)(epoch, "decoder-chain", Instant::now());
                    }
                    if backend.is_none() && !frame.key {
                        (callbacks.dropped)(1);
                        let _ = feedback.send(DecoderFeedback::KeyframeNeeded {
                            local_epoch: epoch,
                            reason: "decoder-awaiting-keyframe".into(),
                        });
                        continue;
                    }
                    if pending.len() >= MAX_QUEUED {
                        (callbacks.dropped)((pending.len() + submitted.len() + 1) as u64);
                        pending.clear();
                        submitted.clear();
                        backend = None;
                        order.reset(epoch);
                        (callbacks.reset)(epoch, "decoder-queue-overflow", Instant::now());
                        let _ = feedback.send(DecoderFeedback::KeyframeNeeded {
                            local_epoch: epoch,
                            reason: "decoder-queue-overflow".into(),
                        });
                        continue;
                    }
                    if backend.is_none() {
                        let codec = match frame.codec {
                            crate::net::Codec::H264 => Codec::H264,
                            crate::net::Codec::Hevc => Codec::Hevc,
                        };
                        let create_started = Instant::now();
                        let created =
                            MfBackend::new(codec, &device, frame.width as u32, frame.height as u32);
                        eprintln!(
                            "BP_TIMING decoder_create {codec:?}: {:.3}ms",
                            create_started.elapsed().as_secs_f64() * 1000.0
                        );
                        match created {
                            Ok(decoder) => backend = Some(decoder),
                            Err(error) => {
                                (callbacks.error)(format!("decoder creation: {error:#}"));
                                (callbacks.dropped)(1);
                                order.reset(epoch);
                                let _ = feedback.send(DecoderFeedback::DecodeError {
                                    local_epoch: epoch,
                                    reason: "decoder-create-error".into(),
                                });
                                continue;
                            }
                        }
                    }
                    ordinal += 1;
                    let meta = FrameMeta {
                        local_epoch: epoch,
                        generation: frame.generation,
                        seq: frame.seq,
                        received_at: frame.received_at,
                        pts_100ns: ordinal * (10_000_000 / 60),
                    };
                    pending.push_back((frame, meta));
                }
            }
        }
        let Some(decoder) = backend.as_mut() else {
            continue;
        };
        let result: Result<()> = (|| {
            // Submit only while the MFT's outstanding outputs are bounded.
            while submitted.len() < MAX_QUEUED {
                let Some((frame, meta)) = pending.front() else {
                    break;
                };
                if !decoder.submit(&frame.data, meta.pts_100ns)? {
                    break;
                }
                let (_, meta) = pending.pop_front().expect("front existed");
                submitted.insert(meta.pts_100ns, meta);
            }
            for _ in 0..MAX_QUEUED {
                let Some(frame) = decoder.poll()? else { break };
                let meta = submitted
                    .remove(&frame.pts_100ns)
                    .context("decoder output has no input timestamp")?;
                let decoded_at = Instant::now();
                let _ = feedback.send(DecoderFeedback::Decoded {
                    local_epoch: meta.local_epoch,
                    generation: meta.generation,
                    seq: meta.seq,
                });
                (callbacks.publish)(frame, meta, decoded_at);
            }
            if pending
                .front()
                .map(|(_, meta)| meta)
                .into_iter()
                .chain(submitted.values())
                .any(|meta| meta.received_at.elapsed() > STALL_LIMIT)
            {
                anyhow::bail!("decoder made no timely progress for 750 ms");
            }
            Ok(())
        })();
        if let Err(error) = result {
            (callbacks.error)(format!("decode: {error:#}"));
            (callbacks.dropped)((pending.len() + submitted.len()) as u64);
            pending.clear();
            submitted.clear();
            backend = None;
            order.reset(epoch);
            (callbacks.reset)(epoch, "decoder-error", Instant::now());
            let _ = feedback.send(DecoderFeedback::DecodeError {
                local_epoch: epoch,
                reason: "decoder-error".into(),
            });
        }
    }
    Ok(())
}
