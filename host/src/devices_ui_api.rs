//! Boundary between the host and the Devices window.
//!
//! Host -> UI is [`DevicesEvent`], UI -> host is [`DevicesCommand`], both over
//! std mpsc channels. The tray's "Devices..." item reaches the host as one of
//! fixed stdin lines: [`OPEN_DEVICES_LINE`] (open the Win32 window),
//! [`SHOW_DEVICES_APP_LINE`] (show the Devices section of the Flutter app) or
//! [`OPEN_FEEDBACK_LINE`] (open the app's report sheet);
//! any other stdin input is ignored.
//!
//! Pairing codes, QR URIs and comparison numbers are secrets for the time they
//! are on screen, so their `Debug` output is redacted and no logger prints them.

use std::fmt::{self, Write as _};
use std::io::{self, BufRead, BufReader, Read};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::SystemTime;

/// The one line the tray writes to the host's stdin for "Devices...".
pub const OPEN_DEVICES_LINE: &[u8] = b"open-devices\n";

/// The line the tray writes when the Flutter app shows the Devices section.
pub const SHOW_DEVICES_APP_LINE: &[u8] = b"show-devices-app\n";

/// The line the tray writes for "Report a problem": the app opens its report sheet.
pub const OPEN_FEEDBACK_LINE: &[u8] = b"open-feedback\n";

/// What a fixed stdin line asks the host for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinRequest {
    /// `open-devices`: open (or raise) the Win32 Devices window.
    OpenDevices,
    /// `show-devices-app`: tell connected app clients to show Devices.
    ShowDevicesApp,
    /// `open-feedback`: tell connected app clients to open the report sheet.
    OpenFeedback,
}

/// Longer stdin lines are discarded unread, so stdin cannot grow memory.
const MAX_STDIN_LINE: usize = 64;

/// One paired device, as the Devices window lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    /// RFC 3339 UTC, as stored in the trusted list. `None` if never seen.
    pub last_seen: Option<String>,
    pub connected: bool,
}

/// Host -> UI.
#[derive(Clone, PartialEq, Eq)]
pub enum DevicesEvent {
    List(Vec<DeviceRow>),
    PairWaiting {
        /// `70Q1K-9MXR1`, shown large on the window.
        code_display: String,
        /// `bubbleplay://pair?...`, drawn as a QR code.
        qr_uri: String,
        pc_id: String,
        expires_at: SystemTime,
    },
    PairExpired,
    PairStopped,
    PairFailed,
    PairRequest {
        id: String,
        name: String,
        /// `482 913`, the number both screens must show.
        sas_display: String,
    },
    PairDone {
        id: String,
        name: String,
    },
    PairDenied,
    SaveError(String),
}

/// UI -> host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicesCommand {
    Open,
    Revoke(String),
    StartPairing,
    CancelPairing,
    /// Allow the pending pair request for this device ID.
    Allow(String),
    /// Deny the pending pair request for this device ID.
    Deny(String),
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Debug for DevicesEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::List(rows) => f.debug_tuple("List").field(&rows.len()).finish(),
            Self::PairWaiting {
                pc_id, expires_at, ..
            } => f
                .debug_struct("PairWaiting")
                .field("code_display", &Redacted)
                .field("qr_uri", &Redacted)
                .field("pc_id", pc_id)
                .field("expires_at", expires_at)
                .finish(),
            Self::PairExpired => f.write_str("PairExpired"),
            Self::PairStopped => f.write_str("PairStopped"),
            Self::PairFailed => f.write_str("PairFailed"),
            Self::PairRequest { id, name, .. } => f
                .debug_struct("PairRequest")
                .field("id", id)
                .field("name", name)
                .field("sas_display", &Redacted)
                .finish(),
            Self::PairDone { id, name } => f
                .debug_struct("PairDone")
                .field("id", id)
                .field("name", name)
                .finish(),
            Self::PairDenied => f.write_str("PairDenied"),
            Self::SaveError(error) => f.debug_tuple("SaveError").field(error).finish(),
        }
    }
}

/// Host end of the channels: sends events, receives commands.
pub struct HostEnds {
    pub events: Sender<DevicesEvent>,
    pub commands: Receiver<DevicesCommand>,
}

/// UI end of the channels: receives events, sends commands. Clone `commands`
/// to give the stdin reader its own sender.
pub struct UiEnds {
    pub events: Receiver<DevicesEvent>,
    pub commands: Sender<DevicesCommand>,
}

pub fn channel() -> (HostEnds, UiEnds) {
    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    (
        HostEnds {
            events: event_tx,
            commands: command_rx,
        },
        UiEnds {
            events: event_rx,
            commands: command_tx,
        },
    )
}

/// What the Devices window implements. The window thread calls `show` for
/// each event in order.
pub trait DevicesUi: Send {
    fn show(&mut self, event: DevicesEvent);
}

/// Logs each event with its secrets redacted. Used until the window exists.
pub struct LogDevicesUi;

impl DevicesUi for LogDevicesUi {
    fn show(&mut self, event: DevicesEvent) {
        match &event {
            DevicesEvent::PairFailed => {
                tracing::info!("Pairing failed: the code didn't match. Open a new code.")
            }
            DevicesEvent::PairStopped => tracing::info!("Pairing stopped by relay or cancelled"),
            DevicesEvent::PairExpired => tracing::info!("Pairing expired"),
            _ => tracing::info!(?event, "devices window event"),
        }
    }
}

/// Drops every event.
pub struct NoopDevicesUi;

impl DevicesUi for NoopDevicesUi {
    fn show(&mut self, _event: DevicesEvent) {}
}

/// Feeds every event to `ui` until the host end is dropped.
pub fn run_ui(mut ui: impl DevicesUi, events: Receiver<DevicesEvent>) {
    for event in events {
        ui.show(event);
    }
}

/// The QR URI for pairing, matching fixture `phase2.pairing.qr.android`.
/// `relay_origin` is the relay origin (`https://example.test`), `pc_id` the
/// canonical PC ID and `code` the canonical pairing code, without the dash.
pub fn pair_qr_uri(relay_origin: &str, pc_id: &str, code: &str) -> String {
    format!(
        "bubbleplay://pair?relay={}&pc={}&code={}",
        encode_component(relay_origin),
        encode_component(pc_id),
        encode_component(code)
    )
}

fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// The request a stdin line makes. Only the exact `open-devices`,
/// `show-devices-app` and `open-feedback` lines (their line ending aside) count; everything else
/// is ignored.
pub fn parse_stdin_request(line: &[u8]) -> Option<StdinRequest> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    match line {
        b"open-devices" => Some(StdinRequest::OpenDevices),
        b"show-devices-app" => Some(StdinRequest::ShowDevicesApp),
        b"open-feedback" => Some(StdinRequest::OpenFeedback),
        _ => None,
    }
}

/// The command a stdin line asks for. Only the exact `open-devices` line
/// (its line ending aside) counts; everything else is ignored.
pub fn parse_stdin_line(line: &[u8]) -> Option<DevicesCommand> {
    (parse_stdin_request(line) == Some(StdinRequest::OpenDevices)).then_some(DevicesCommand::Open)
}

/// Reads host stdin lines on a thread and forwards each command. The thread
/// ends at end of input or when the command receiver is gone.
pub fn spawn_stdin_reader<R: Read + Send + 'static>(
    reader: R,
    commands: Sender<DevicesCommand>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("devices-stdin".into())
        .spawn(move || {
            read_lines(reader, |line| match parse_stdin_line(line) {
                Some(command) => commands.send(command).is_ok(),
                None => true,
            })
        })
}

/// Reads host stdin lines on a thread and forwards each [`StdinRequest`].
/// The thread ends at end of input or when the receiver is gone.
pub fn spawn_stdin_request_reader<R: Read + Send + 'static>(
    reader: R,
    requests: Sender<StdinRequest>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("devices-stdin".into())
        .spawn(move || {
            read_lines(reader, |line| match parse_stdin_request(line) {
                Some(request) => requests.send(request).is_ok(),
                None => true,
            })
        })
}

/// Feeds each stdin line of at most [`MAX_STDIN_LINE`] bytes to `line_fn`,
/// which returns false to stop reading.
fn read_lines<R: Read>(reader: R, mut line_fn: impl FnMut(&[u8]) -> bool) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::with_capacity(MAX_STDIN_LINE + 1);
    loop {
        line.clear();
        // One byte past the limit tells an over-long line from a short one.
        let read = match (&mut reader)
            .take(MAX_STDIN_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let complete = line.last() == Some(&b'\n');
        if !complete && read > MAX_STDIN_LINE {
            if discard_line(&mut reader).is_err() {
                break;
            }
            continue;
        }
        if !line_fn(&line) {
            break;
        }
        if !complete {
            // End of input without a line ending.
            break;
        }
    }
}

fn discard_line<R: BufRead>(reader: &mut R) -> io::Result<()> {
    loop {
        let buffered = reader.fill_buf()?;
        if buffered.is_empty() {
            return Ok(());
        }
        match buffered.iter().position(|&byte| byte == b'\n') {
            Some(index) => {
                reader.consume(index + 1);
                return Ok(());
            }
            None => {
                let len = buffered.len();
                reader.consume(len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    fn fixture() -> serde_json::Value {
        let text = include_str!("../../fixtures/device-keys-contract.json");
        serde_json::from_str(text).expect("device-keys fixture parses")
    }

    fn read_all(input: &[u8]) -> Vec<DevicesCommand> {
        let (tx, rx) = mpsc::channel();
        let handle = spawn_stdin_reader(Cursor::new(input.to_vec()), tx).expect("spawn");
        handle.join().expect("reader thread");
        rx.try_iter().collect()
    }

    #[test]
    fn only_the_exact_open_devices_line_is_a_command() {
        assert_eq!(
            parse_stdin_line(b"open-devices\n"),
            Some(DevicesCommand::Open)
        );
        assert_eq!(
            parse_stdin_line(b"open-devices\r\n"),
            Some(DevicesCommand::Open)
        );
        assert_eq!(
            parse_stdin_line(b"open-devices"),
            Some(DevicesCommand::Open)
        );
        let others: [&[u8]; 7] = [
            b"\n",
            b"Open-Devices\n",
            b"open-devices \n",
            b"open-devices extra\n",
            b"open-devices\n\n",
            b"revoke ABCD\n",
            b"allow\n",
        ];
        for other in others {
            assert_eq!(parse_stdin_line(other), None, "{other:?}");
        }
    }

    fn read_requests(input: &[u8]) -> Vec<StdinRequest> {
        let (tx, rx) = mpsc::channel();
        let handle = spawn_stdin_request_reader(Cursor::new(input.to_vec()), tx).expect("spawn");
        handle.join().expect("reader thread");
        rx.try_iter().collect()
    }

    #[test]
    fn only_the_exact_lines_are_requests() {
        assert_eq!(
            parse_stdin_request(SHOW_DEVICES_APP_LINE),
            Some(StdinRequest::ShowDevicesApp)
        );
        assert_eq!(
            parse_stdin_request(b"show-devices-app\r\n"),
            Some(StdinRequest::ShowDevicesApp)
        );
        assert_eq!(
            parse_stdin_request(b"show-devices-app"),
            Some(StdinRequest::ShowDevicesApp)
        );
        assert_eq!(
            parse_stdin_request(OPEN_DEVICES_LINE),
            Some(StdinRequest::OpenDevices)
        );
        let others: [&[u8]; 6] = [
            b"Show-Devices-App\n",
            b"show-devices-app \n",
            b" show-devices-app\n",
            b"show-devices-app extra\n",
            b"show-devices\n",
            b"show-devices-app\n\n",
        ];
        for other in others {
            assert_eq!(parse_stdin_request(other), None, "{other:?}");
            assert_eq!(parse_stdin_line(other), None, "{other:?}");
        }
        assert_eq!(
            parse_stdin_request(OPEN_FEEDBACK_LINE),
            Some(StdinRequest::OpenFeedback)
        );
        assert_eq!(parse_stdin_line(OPEN_FEEDBACK_LINE), None);
        assert_eq!(parse_stdin_request(b"open-feedback x\n"), None);
        // The show line is never a window command.
        assert_eq!(parse_stdin_line(SHOW_DEVICES_APP_LINE), None);
    }

    #[test]
    fn request_reader_forwards_both_lines_in_order() {
        let input = b"noise\nshow-devices-app\nopen-devices\nshow-devices-app-x\nshow-devices-app\r\nshow-devices-app";
        assert_eq!(
            read_requests(input),
            vec![
                StdinRequest::ShowDevicesApp,
                StdinRequest::OpenDevices,
                StdinRequest::ShowDevicesApp,
                StdinRequest::ShowDevicesApp,
            ]
        );
    }

    #[test]
    fn request_reader_skips_over_long_lines() {
        let mut input = vec![b'x'; 5000];
        input.extend_from_slice(b"show-devices-app\nshow-devices-app\n");
        assert_eq!(read_requests(&input), vec![StdinRequest::ShowDevicesApp]);
    }

    #[test]
    fn command_reader_ignores_the_show_line() {
        assert_eq!(
            read_all(b"show-devices-app\nopen-devices\n"),
            vec![DevicesCommand::Open]
        );
    }

    #[test]
    fn reader_forwards_only_exact_lines_and_stops_at_eof() {
        let input =
            b"noise\nopen-devices\nopen-devices-x\n  open-devices\nopen-devices\r\nopen-devices";
        assert_eq!(
            read_all(input),
            vec![
                DevicesCommand::Open,
                DevicesCommand::Open,
                DevicesCommand::Open
            ]
        );
    }

    #[test]
    fn reader_skips_over_long_lines_and_keeps_going() {
        let mut input = vec![b'x'; 5000];
        input.extend_from_slice(b"\nopen-devices\n");
        assert_eq!(read_all(&input), vec![DevicesCommand::Open]);
    }

    #[test]
    fn reader_ends_when_the_command_receiver_is_gone() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let handle = spawn_stdin_reader(Cursor::new(b"open-devices\nopen-devices\n".to_vec()), tx)
            .expect("spawn");
        handle.join().expect("reader thread");
    }

    #[test]
    fn qr_uri_matches_the_fixture() {
        let fixture = fixture();
        let pairing = &fixture["phase2"]["pairing"];
        let expected = pairing["qr"]["android"].as_str().expect("qr fixture");
        let uri = pair_qr_uri(
            "https://example.test",
            pairing["pcId"].as_str().expect("pcId"),
            pairing["code"].as_str().expect("code"),
        );
        assert_eq!(uri, expected);
    }

    #[test]
    fn qr_uri_escapes_the_relay_origin() {
        assert_eq!(
            pair_qr_uri("https://relay.example:8443", "AAAA", "BBBB"),
            "bubbleplay://pair?relay=https%3A%2F%2Frelay.example%3A8443&pc=AAAA&code=BBBB"
        );
    }

    #[test]
    fn debug_output_hides_the_code_qr_and_sas() {
        let waiting = DevicesEvent::PairWaiting {
            code_display: "70Q1K-9MXR1".into(),
            qr_uri: "bubbleplay://pair?code=70Q1K9MXR1".into(),
            pc_id: "77VH-7M56-8GJK-Y12J".into(),
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(300),
        };
        let request = DevicesEvent::PairRequest {
            id: "47Z3-3QX1-AJH6-2RKB".into(),
            name: "Pixel 8".into(),
            sas_display: "830 687".into(),
        };
        for event in [waiting, request] {
            let text = format!("{event:?}");
            assert!(!text.contains("70Q1K"), "{text}");
            assert!(!text.contains("bubbleplay://"), "{text}");
            assert!(!text.contains("830 687"), "{text}");
            assert!(text.contains("<redacted>"), "{text}");
        }
        let done = DevicesEvent::PairDone {
            id: "47Z3-3QX1-AJH6-2RKB".into(),
            name: "Pixel 8".into(),
        };
        assert!(format!("{done:?}").contains("Pixel 8"));
    }

    #[test]
    fn channels_carry_events_and_commands_both_ways() {
        let (host, ui) = channel();
        host.events.send(DevicesEvent::PairDenied).expect("event");
        assert_eq!(ui.events.recv().expect("event"), DevicesEvent::PairDenied);
        ui.commands
            .send(DevicesCommand::Revoke("ID".into()))
            .expect("command");
        assert_eq!(
            host.commands.recv().expect("command"),
            DevicesCommand::Revoke("ID".into())
        );
    }

    fn two_events() -> Receiver<DevicesEvent> {
        let (tx, rx) = mpsc::channel();
        tx.send(DevicesEvent::PairExpired).expect("event");
        tx.send(DevicesEvent::SaveError("disk full".into()))
            .expect("event");
        rx
    }

    #[test]
    fn no_op_and_log_uis_drain_every_event() {
        run_ui(NoopDevicesUi, two_events());
        run_ui(LogDevicesUi, two_events());
    }
}
