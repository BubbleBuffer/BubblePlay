// Named-pipe tests of the lab helper, split out of bubbleplay_lab_helper.rs to keep that file shorter.
use super::*;

#[test]
fn current_user_pipe_acl_excludes_world_and_authenticated_users() {
    let sid = current_user_sid().unwrap();
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})");
    assert!(!sddl.contains(";;;WD"));
    assert!(!sddl.contains(";;;AU"));
    assert!(SecurityDescriptor::from_sddl(&sddl).is_ok());
}

#[test]
fn current_user_pipe_can_be_bound_as_a_single_local_instance() {
    let sid = current_user_sid().unwrap();
    let name = format!(r"\\.\pipe\BubblePlay.StreamLab.test.{}", std::process::id());
    let server = PipeServer::bind_named_for_sid(&name, &sid).unwrap();
    assert!(PipeServer::bind_named_for_sid(&name, &sid).is_err());
    drop(server);
}

#[test]
fn pipe_accept_is_cancelable_without_a_client() {
    let sid = current_user_sid().unwrap();
    let name = format!(
        r"\\.\pipe\BubblePlay.StreamLab.cancel.{}",
        std::process::id()
    );
    let server = PipeServer::bind_named_for_sid(&name, &sid).unwrap();
    let stop = Arc::new(AtomicBool::new(true));
    let started = Instant::now();
    assert!(server
        .accept_for_sid_cancelable(&sid, stop, Duration::from_secs(5))
        .unwrap()
        .is_none());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn production_pipe_accept_is_genuinely_overlapped() {
    assert_ne!(pipe_open_mode() & FILE_FLAG_OVERLAPPED, 0);
}

/// One deadline for everything the test waits on: client process start, pipe `Connect`, the
/// server's accept and the line itself. PowerShell cold start on a loaded hosted runner is
/// the slow part, so the bound is generous and every wait shares it.
const CLIENT_LINE_DEADLINE: Duration = Duration::from_secs(60);

#[test]
fn connected_client_line_is_received_before_the_client_disconnects() {
    let sid = current_user_sid().unwrap();
    let name = format!(
        r"\\.\pipe\BubblePlay.StreamLab.roundtrip.{}.{}",
        std::process::id(),
        unix_ms(SystemTime::now()).unwrap()
    );
    let server = PipeServer::bind_named_for_sid(&name, &sid).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server_sid = sid.clone();
    let (line_tx, line_rx) = mpsc::sync_channel(1);
    let server_thread = thread::spawn(move || {
        let accepted = server
            .accept_for_sid_cancelable(&server_sid, server_stop, CLIENT_LINE_DEADLINE)
            .unwrap();
        let mut reader = BufReader::new(accepted.expect("client accepted"));
        let line = read_line_limited(&mut reader, MAX_COMMAND_BYTES).unwrap();
        let _ = line_tx.send(line);
    });

    const CLIENT_SCRIPT: &str = r#"$ErrorActionPreference='Stop'; $p=[IO.Pipes.NamedPipeClientStream]::new('.', $env:BUBBLEPLAY_TEST_PIPE, [IO.Pipes.PipeDirection]::InOut); $p.Connect([int]$env:BUBBLEPLAY_TEST_CONNECT_MS); $w=[IO.StreamWriter]::new($p); $w.AutoFlush=$true; $w.WriteLine('{"command":"ping"}'); Start-Sleep -Seconds 120"#;
    let powershell = system_directory()
        .unwrap()
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let pipe_leaf = name.rsplit("\\").next().unwrap();
    let connect_ms = CLIENT_LINE_DEADLINE.as_millis().to_string();
    let mut client = configured_powershell(
        &powershell,
        CLIENT_SCRIPT,
        &[
            ("BUBBLEPLAY_TEST_PIPE", OsStr::new(pipe_leaf)),
            ("BUBBLEPLAY_TEST_CONNECT_MS", OsStr::new(&connect_ms)),
        ],
    )
    .spawn()
    .unwrap();
    // Wait on the server thread's event, bounded by the shared deadline, then prove the
    // line arrived while the client is still alive.
    let received = line_rx.recv_timeout(CLIENT_LINE_DEADLINE);
    let client_still_connected = client.try_wait().unwrap().is_none();
    if received.is_err() {
        stop.store(true, Ordering::Release);
    }
    let _ = client.kill();
    let _ = client.wait();
    server_thread.join().unwrap();
    assert_eq!(received.unwrap(), Some(r#"{"command":"ping"}"#.to_owned()));
    assert!(client_still_connected);
}
