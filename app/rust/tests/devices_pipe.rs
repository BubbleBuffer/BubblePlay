//! The Devices pipe client against a fake server made here with `CreateNamedPipe` on a unique
//! test pipe name. Nothing touches the real host pipe.
#![cfg(windows)]

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bubbleplay_app_core::devices_pipe::{
    connected_line, disconnected_line, pipe_name_for_current_user, Config, DevicesClient, Reason,
    MAX_LINE,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_NO_DATA, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};

const WAIT: Duration = Duration::from_secs(15);

/// A hung test must not hang the run: the whole binary ends after this.
fn watchdog() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(240));
            eprintln!("devices_pipe tests: watchdog fired");
            std::process::exit(2);
        });
    });
}

fn test_pipe_name(tag: &str) -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        r"\\.\pipe\bubbleplay-devices-test-{}-{}-{tag}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// One server-side pipe instance, not yet connected.
fn create_instance(name: &str) -> HANDLE {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL terminated.
    unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            4,
            64 * 1024,
            64 * 1024,
            0,
            None,
        )
    }
}

/// A listening server instance, created before the client starts so the client's first attempt
/// finds it.
struct Listener(HANDLE);

fn listen(name: &str) -> Listener {
    let pipe = create_instance(name);
    assert!(!pipe.is_invalid(), "CreateNamedPipe failed");
    Listener(pipe)
}

/// Waits for the client and returns the connection. Used sequentially (write, then read): the
/// handle is synchronous, so a read in one thread would block a write in another.
fn accept_on(listener: Listener) -> File {
    let pipe = listener.0;
    // SAFETY: a valid pipe handle. A client that connected early is reported as
    // ERROR_PIPE_CONNECTED, one that connected and already hung up as ERROR_NO_DATA.
    if let Err(e) = unsafe { ConnectNamedPipe(pipe, None) } {
        assert!(
            e.code() == ERROR_PIPE_CONNECTED.to_hresult() || e.code() == ERROR_NO_DATA.to_hresult(),
            "ConnectNamedPipe: {e}"
        );
    }
    // SAFETY: the handle is owned here and handed to the File.
    unsafe { File::from_raw_handle(pipe.0 as RawHandle) }
}

/// Creates an instance now and waits for the client on it (a server that comes up later).
fn accept(name: &str) -> File {
    accept_on(listen(name))
}

/// A client with fast retries and a sink that forwards into a channel.
fn start(name: &str, config: Config) -> (DevicesClient, Receiver<String>) {
    watchdog();
    let (tx, rx) = channel();
    let sink = Arc::new(move |line: String| tx.send(line).is_ok());
    (DevicesClient::start(name.to_owned(), sink, config), rx)
}

fn fast() -> Config {
    Config {
        backoff_initial: Duration::from_millis(30),
        backoff_max: Duration::from_millis(120),
        trusted_sid: None,
    }
}

fn next(rx: &Receiver<String>) -> String {
    rx.recv_timeout(WAIT).expect("no line from the client")
}

/// Skips lines until `want` arrives.
fn wait_for(rx: &Receiver<String>, want: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line == want => return,
            Ok(_) => {}
            Err(_) => panic!("never saw {want}"),
        }
    }
}

const HELLO: &str = r#"{"type":"hello","protocol":1,"pcName":"Desk","pcId":"ABC","relayOrigin":"https://r.example","relayOnline":true}"#;
const LIST: &str = r#"{ "type":"list", "devices":[{"id":"d1","name":"Pixel \u00e4","lastSeen":null,"connected":false}] }"#;

#[test]
fn connects_and_passes_lines_verbatim() {
    let name = test_pipe_name("verbatim");
    let listener = listen(&name);
    let (client, rx) = start(&name, fast());
    let mut server = accept_on(listener);
    server
        .write_all(format!("{HELLO}\n{LIST}\n").as_bytes())
        .unwrap();
    assert_eq!(next(&rx), connected_line());
    assert_eq!(next(&rx), HELLO);
    assert_eq!(next(&rx), LIST);
    drop(client);
}

#[test]
fn sends_a_valid_command_exactly() {
    let name = test_pipe_name("send");
    let listener = listen(&name);
    let (client, rx) = start(&name, fast());
    let server = accept_on(listener);
    wait_for(&rx, &connected_line());
    let command = r#"{"type":"revoke", "id":"d1"}"#;
    client.send(command).unwrap();
    client.send(r#"{"type":"open"}"#).unwrap();
    let mut reader = BufReader::new(server);
    let mut got = String::new();
    reader.read_line(&mut got).unwrap();
    assert_eq!(got, format!("{command}\n"));
    got.clear();
    reader.read_line(&mut got).unwrap();
    assert_eq!(got, "{\"type\":\"open\"}\n");
}

#[test]
fn rejects_invalid_commands_without_writing() {
    let name = test_pipe_name("invalid");
    let listener = listen(&name);
    let (client, rx) = start(&name, fast());
    let server = accept_on(listener);
    wait_for(&rx, &connected_line());
    for bad in [
        "",
        "open",
        r#"{"type":"nope"}"#,
        r#"{"type":"allow"}"#,
        r#"{"type":"deny","id":3}"#,
        "{\"type\":\"open\"}\n{\"type\":\"open\"}",
    ] {
        assert!(client.send(bad).is_err(), "{bad:?}");
    }
    let too_long = format!(r#"{{"type":"open","x":"{}"}}"#, "x".repeat(MAX_LINE));
    assert!(client.send(&too_long).is_err());
    // The first thing the server sees is the valid command that follows.
    client.send(r#"{"type":"cancelPairing"}"#).unwrap();
    let mut got = String::new();
    BufReader::new(server).read_line(&mut got).unwrap();
    assert_eq!(got, "{\"type\":\"cancelPairing\"}\n");
}

#[test]
fn refuses_commands_while_not_connected() {
    let name = test_pipe_name("notconnected");
    let (client, rx) = start(&name, fast());
    wait_for(&rx, &disconnected_line(Reason::HostNotRunning));
    assert!(client.send(r#"{"type":"open"}"#).is_err());
}

fn protocol_violation(tag: &str, bytes: &[u8]) {
    let name = test_pipe_name(tag);
    let listener = listen(&name);
    let (client, rx) = start(&name, fast());
    let mut server = accept_on(listener);
    server.write_all(format!("{HELLO}\n").as_bytes()).unwrap();
    assert_eq!(next(&rx), connected_line());
    assert_eq!(next(&rx), HELLO);
    // The client may hang up before the whole violation is written.
    let _ = server.write_all(bytes);
    assert_eq!(next(&rx), disconnected_line(Reason::Protocol), "{tag}");
    // The client hung up: the server sees the end of the stream.
    let mut rest = Vec::new();
    server.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
    drop(client);
}

#[test]
fn overlong_line_drops_the_connection() {
    let line = format!("{{\"a\":\"{}\"}}\n", "x".repeat(MAX_LINE));
    protocol_violation("long", line.as_bytes());
}

#[test]
fn overlong_data_without_newline_drops_the_connection() {
    protocol_violation("longraw", &vec![b'a'; MAX_LINE + 1]);
}

#[test]
fn bad_utf8_drops_the_connection() {
    protocol_violation("utf8", b"{\"a\":\"\xff\xfe\"}\n");
}

#[test]
fn non_object_json_drops_the_connection() {
    protocol_violation("array", b"[1,2]\n");
    protocol_violation("string", b"\"hello\"\n");
    protocol_violation("garbage", b"not json\n");
}

#[test]
fn reconnects_after_the_server_closes_and_restarts() {
    let name = test_pipe_name("reconnect");
    let listener = listen(&name);
    let (_client, rx) = start(&name, fast());
    let mut server = accept_on(listener);
    server.write_all(format!("{HELLO}\n").as_bytes()).unwrap();
    assert_eq!(next(&rx), connected_line());
    assert_eq!(next(&rx), HELLO);
    drop(server);
    wait_for(&rx, &disconnected_line(Reason::Io));
    // The host comes back (after possibly a few not-running retries).
    let mut again = accept(&name);
    again.write_all(format!("{LIST}\n").as_bytes()).unwrap();
    wait_for(&rx, &connected_line());
    assert_eq!(next(&rx), LIST);
}

#[test]
fn reports_host_not_running_once() {
    let name = test_pipe_name("absent");
    let (client, rx) = start(&name, fast());
    assert_eq!(next(&rx), disconnected_line(Reason::HostNotRunning));
    // Many retry periods pass without another line.
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(900)),
        Err(RecvTimeoutError::Timeout)
    );
    drop(client);
}

#[test]
fn server_of_another_user_is_rejected_before_any_byte() {
    let name = test_pipe_name("untrusted");
    let other_sid = vec![1u8, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0]; // S-1-5-18, not us
    let listener = listen(&name);
    let (client, rx) = start(
        &name,
        Config {
            trusted_sid: Some(other_sid),
            ..fast()
        },
    );
    let mut server = accept_on(listener);
    // The client may already have hung up, so the write can fail.
    let _ = server.write_all(format!("{HELLO}\n").as_bytes());
    assert_eq!(next(&rx), disconnected_line(Reason::NotTrusted));
    assert!(client.send(r#"{"type":"open"}"#).is_err());
    let mut got = Vec::new();
    server.read_to_end(&mut got).unwrap();
    assert!(got.is_empty(), "the client wrote to an untrusted server");
    // The server's hello was never passed on.
    while let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
        assert_ne!(line, connected_line());
        assert_ne!(line, HELLO);
    }
}

#[test]
fn stop_is_prompt_and_closes_the_pipe() {
    let name = test_pipe_name("stop");
    let listener = listen(&name);
    let (mut client, rx) = start(&name, fast());
    let mut server = accept_on(listener);
    wait_for(&rx, &connected_line());
    let began = Instant::now();
    client.stop();
    assert!(began.elapsed() < Duration::from_secs(3));
    let mut rest = Vec::new();
    server.read_to_end(&mut rest).unwrap();
    // Nothing more arrives, and stopping twice is harmless.
    client.stop();
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(300)),
        Err(RecvTimeoutError::Disconnected)
    );
}

#[test]
fn stop_while_waiting_to_reconnect() {
    let name = test_pipe_name("stopidle");
    let (client, rx) = start(
        &name,
        Config {
            backoff_initial: Duration::from_secs(30),
            backoff_max: Duration::from_secs(30),
            trusted_sid: None,
        },
    );
    assert_eq!(next(&rx), disconnected_line(Reason::HostNotRunning));
    let began = Instant::now();
    drop(client);
    assert!(began.elapsed() < Duration::from_secs(3));
}

#[test]
fn replacing_a_client_silences_the_old_one() {
    let name = test_pipe_name("replace");
    let listener = listen(&name);
    let (old, old_rx) = start(&name, fast());
    let mut first = accept_on(listener);
    wait_for(&old_rx, &connected_line());
    drop(old);
    let mut rest = Vec::new();
    first.read_to_end(&mut rest).unwrap();

    let listener = listen(&name);
    let (new, new_rx) = start(&name, fast());
    let mut second = accept_on(listener);
    second.write_all(format!("{HELLO}\n").as_bytes()).unwrap();
    assert_eq!(next(&new_rx), connected_line());
    assert_eq!(next(&new_rx), HELLO);
    // The old client's sink got nothing after it was dropped.
    assert!(old_rx.try_iter().all(|line| line != HELLO));
    drop(new);
}

#[test]
fn a_gone_sink_stops_the_client() {
    watchdog();
    let name = test_pipe_name("sinkgone");
    let sink = Arc::new(|_: String| false);
    let mut client = DevicesClient::start(name.clone(), sink, fast());
    // The first report returns false, so the supervisor ends; stop returns at once.
    let began = Instant::now();
    client.stop();
    assert!(began.elapsed() < Duration::from_secs(3));
}

#[test]
fn the_real_pipe_name_carries_the_user_sid() {
    let name = pipe_name_for_current_user().unwrap();
    let sid = name
        .strip_prefix(r"\\.\pipe\bubbleplay-devices-v1-")
        .expect("prefix");
    assert!(sid.starts_with("S-1-"), "{sid}");
}
