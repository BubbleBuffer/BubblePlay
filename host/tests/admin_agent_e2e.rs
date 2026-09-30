//! End-to-end: a non-elevated `--dev` admin agent in a scratch data dir
//! receives input from the host's `InputSink` over the pipe, enforces the
//! hello/version/validation rules, keeps its pipe to itself, and exits when
//! the (simulated) tray stops it or disappears.
//!
//! The only real input injected is a 1-pixel relative mouse move and its
//! reverse.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use bubbleplay_host::admin_agent::client::{
    probe, AdminAgentLink, LinkError, LinkState, ProbeResult, PIPE_CLIENT_ACCESS,
};
use bubbleplay_host::admin_agent::protocol::{
    pipe_name, scoped_name, Record, AGENT_STOP_EVENT, MOUSEEVENTF_MOVE, PROTOCOL_MAX, TRAY_MUTEX,
};
use bubbleplay_host::admin_agent::trust;
use bubbleplay_host::input::InputSink;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent};

struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: owned handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// The named objects a running tray owns.
struct FakeTray {
    _mutex: Owned,
    stop: Owned,
}

impl FakeTray {
    fn new(dir: &Path) -> Self {
        let mutex = wide(&scoped_name(TRAY_MUTEX, Some(dir.as_os_str())));
        let event = wide(&scoped_name(AGENT_STOP_EVENT, Some(dir.as_os_str())));
        // SAFETY: NUL-terminated names.
        unsafe {
            Self {
                _mutex: Owned(CreateMutexW(None, true, PCWSTR(mutex.as_ptr())).unwrap()),
                stop: Owned(CreateEventW(None, true, false, PCWSTR(event.as_ptr())).unwrap()),
            }
        }
    }

    fn stop_agent(&self) {
        // SAFETY: valid event handle.
        unsafe { SetEvent(self.stop.0) }.unwrap();
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bubbleplay-agent-e2e-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn_agent(dir: &Path, extra: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_bubbleplay_admin_agent"))
        .arg("--dev")
        .args(extra)
        .env("BUBBLEPLAY_TRAY_DATA_DIR", dir)
        .spawn()
        .unwrap()
}

fn wait_for<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = check() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_exit(child: &mut Child, timeout: Duration) -> Option<i32> {
    wait_for(timeout, || child.try_wait().unwrap().map(|s| s.code().unwrap_or(-1)))
}

fn log_text(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("logs").join("admin-agent.log")).unwrap_or_default()
}

fn open_raw(pipe: &str) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .access_mode(PIPE_CLIENT_ACCESS)
        .open(pipe)
        .unwrap()
}

#[test]
fn dev_agent_receives_host_input_over_the_pipe() {
    if trust::is_process_elevated() {
        eprintln!("skipped: the dev agent refuses to run elevated (run tests unelevated)");
        return;
    }
    let dir = scratch("main");
    let session = trust::current_session_id();
    let pipe = pipe_name(session, Some(dir.as_os_str()));

    // Without a tray the agent exits immediately.
    let mut orphan = spawn_agent(&dir, &[]);
    assert_eq!(wait_exit(&mut orphan, Duration::from_secs(10)), Some(0));
    assert!(log_text(&dir).contains("tray is not running"));

    let tray = FakeTray::new(&dir);
    let mut agent = spawn_agent(&dir, &[]);
    let handshake = wait_for(Duration::from_secs(10), || match probe(&pipe, true) {
        ProbeResult::Ready(handshake) => Some(handshake),
        _ => None,
    })
    .unwrap_or_else(|| panic!("dev agent did not come up: {:?}", probe(&pipe, true)));
    assert_eq!(handshake.version, PROTOCOL_MAX);
    assert!(!handshake.elevated);

    // A production link (Program Files required) refuses the dev agent.
    let mut strict = AdminAgentLink::new(pipe.clone(), false);
    let one = Record::Mouse {
        dx: 1,
        dy: 0,
        data: 0,
        flags: MOUSEEVENTF_MOVE,
    };
    assert_eq!(strict.inject(&[one]), None);
    assert!(
        matches!(strict.state(), LinkState::Fallback(LinkError::Untrusted(_))),
        "{:?}",
        strict.state()
    );

    // Host path: browser packets -> InputSink -> agent -> SendInput.
    {
        let link = AdminAgentLink::new(pipe.clone(), true);
        let mut sink = InputSink::new().with_admin_agent(Some(link));
        // TYPE_MOUSE_REL: +1 then -1 pixel.
        let right = sink.handle(&[2, 1, 0, 0, 0]);
        let left = sink.handle(&[2, 0xFF, 0xFF, 0, 0]);
        assert_eq!(right.application.requested, 1);
        assert_eq!(left.application.requested, 1);
    }
    let log = wait_for(Duration::from_secs(5), || {
        let log = log_text(&dir);
        log.contains("records=2").then_some(log)
    })
    .unwrap_or_else(|| panic!("agent did not report the host's records:\n{}", log_text(&dir)));
    assert!(log.contains("client connected"));
    assert!(log.contains("hello accepted"));

    // Malformed frames are refused and the agent keeps serving.
    let mut raw = open_raw(&pipe);
    raw.write_all(b"XXXXXXXXXXXX").unwrap();
    let mut rest = Vec::new();
    let _ = raw.read_to_end(&mut rest);
    drop(raw);
    // Input before hello is refused with ExpectedHello and closed.
    let mut raw = open_raw(&pipe);
    raw.write_all(b"BPAG\x03\x00\x00\x00").unwrap();
    let mut reply = Vec::new();
    let _ = raw.read_to_end(&mut reply);
    assert_eq!(&reply[..5], b"BPAG\x83", "{reply:?}");
    assert_eq!(reply[8], 2, "expected-hello refusal");
    drop(raw);
    // A client that never says hello is dropped.
    let mut silent = open_raw(&pipe);
    let started = Instant::now();
    let mut buffer = [0u8; 1];
    let read = silent.read(&mut buffer);
    assert!(matches!(read, Ok(0) | Err(_)));
    assert!(started.elapsed() < Duration::from_secs(10));
    drop(silent);
    assert!(matches!(probe(&pipe, true), ProbeResult::Ready(_)));

    // Admin mode off: the tray signals stop and the agent exits.
    tray.stop_agent();
    assert_eq!(wait_exit(&mut agent, Duration::from_secs(10)), Some(0));
    assert!(log_text(&dir).contains("tray requested stop"));
    assert!(matches!(probe(&pipe, true), ProbeResult::Unavailable(_)));
    drop(tray);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn incompatible_agent_makes_the_host_fall_back() {
    if trust::is_process_elevated() {
        eprintln!("skipped: the dev agent refuses to run elevated (run tests unelevated)");
        return;
    }
    let dir = scratch("newer");
    let pipe = pipe_name(trust::current_session_id(), Some(dir.as_os_str()));
    let tray = FakeTray::new(&dir);
    let newer = format!("{}-{}", PROTOCOL_MAX + 1, PROTOCOL_MAX + 2);
    let mut agent = spawn_agent(&dir, &["--dev-protocol-range", &newer]);
    let result = wait_for(Duration::from_secs(10), || match probe(&pipe, true) {
        ProbeResult::Unavailable(_) => None,
        other => Some(other),
    })
    .expect("dev agent did not come up");
    assert_eq!(
        result,
        ProbeResult::Incompatible {
            agent_min: PROTOCOL_MAX + 1,
            agent_max: PROTOCOL_MAX + 2
        }
    );
    let mut link = AdminAgentLink::new(pipe.clone(), true);
    let one = Record::Mouse {
        dx: 0,
        dy: 0,
        data: 0,
        flags: MOUSEEVENTF_MOVE,
    };
    // None: the caller (InputSink) injects at user level instead.
    assert_eq!(link.inject(&[one]), None);
    assert!(
        matches!(link.state(), LinkState::Fallback(LinkError::Incompatible { .. })),
        "{:?}",
        link.state()
    );
    // The tray going away (mutex gone) also stops the agent.
    drop(tray);
    assert_eq!(wait_exit(&mut agent, Duration::from_secs(10)), Some(0));
    assert!(log_text(&dir).contains("tray disappeared"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_refuses_to_run_unelevated_without_dev() {
    let dir = scratch("nodev");
    let mut child = Command::new(env!("CARGO_BIN_EXE_bubbleplay_admin_agent"))
        .env("BUBBLEPLAY_TRAY_DATA_DIR", &dir)
        .spawn()
        .unwrap();
    let code = wait_exit(&mut child, Duration::from_secs(10));
    if trust::is_process_elevated() {
        // Elevated from target\ (not Program Files): refused as well.
        assert_eq!(code, Some(1));
    } else {
        assert_eq!(code, Some(1));
        assert!(log_text(&dir).contains("only --dev instances"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_refuses_a_pipe_name_someone_else_created() {
    if trust::is_process_elevated() {
        eprintln!("skipped: the dev agent refuses to run elevated (run tests unelevated)");
        return;
    }
    let dir = scratch("squat");
    let pipe = pipe_name(trust::current_session_id(), Some(dir.as_os_str()));
    let name = wide(&pipe);
    // A same-user process creates the agent's pipe name first.
    // SAFETY: NUL-terminated name; default security.
    let squat = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            4,
            4096,
            4096,
            0,
            None,
        )
    };
    assert!(!squat.is_invalid());
    let _squat = Owned(squat);
    let tray = FakeTray::new(&dir);
    let mut agent = spawn_agent(&dir, &[]);
    assert_eq!(wait_exit(&mut agent, Duration::from_secs(10)), Some(1));
    assert!(log_text(&dir).contains("name taken"), "{}", log_text(&dir));
    drop(tray);
    let _ = std::fs::remove_dir_all(&dir);
}
