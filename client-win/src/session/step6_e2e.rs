//! Opt-in recorded-stream qualification. Both child processes are headless,
//! bounded, loopback-only and joined; input is synthetic InputLink traffic.

use std::{
    fs::{self, File},
    net::{Ipv4Addr, TcpListener, TcpStream},
    os::windows::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};

// Lifetimes in seconds. The client does MF, device, audio and preflight setup
// before its --exit-after clock; the host's clock starts before client launch.
// The original 16 s host expired while the 11 s client loop was still running:
// process startup and final cleanup extend beyond that loop's clock. Bound the
// whole child lifetime, so slow setup fails explicitly instead of hanging.
const HOST_BIND_BUDGET_S: u64 = 60; // fake-host startup loads recordings; seen at 9-20+ s under load
const CLIENT_SETUP_BUDGET_S: u64 = 40; // cold process/device/MF startup
const ESTABLISH_BUDGET_S: u64 = 20; // WSS/ICE establishment inside the client clock
const EXERCISE_S: u64 = 11; // monitor switch and Game/Work round trip after establishment
const TEARDOWN_BUDGET_S: u64 = 10; // client shutdown after its clock
const HOST_MARGIN_S: u64 = 15; // the host must outlive the client's whole bounded life
const CLIENT_EXIT_S: u64 = ESTABLISH_BUDGET_S + EXERCISE_S;
const CLIENT_DEADLINE_S: u64 = CLIENT_SETUP_BUDGET_S + CLIENT_EXIT_S + TEARDOWN_BUDGET_S;
const HOST_EXIT_S: u64 = HOST_BIND_BUDGET_S + CLIENT_DEADLINE_S + HOST_MARGIN_S;
const HOST_DEADLINE_S: u64 = HOST_BIND_BUDGET_S + HOST_EXIT_S + HOST_MARGIN_S;

struct OwnedChild(Child);

impl OwnedChild {
    fn wait(&mut self, duration: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + duration;
        loop {
            if let Some(status) = self.0.try_wait().expect("query owned child") {
                return status;
            }
            assert!(Instant::now() < deadline, "owned child exceeded deadline");
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn launch(exe: &Path, args: &[String], root: &Path, name: &str) -> OwnedChild {
    OwnedChild(
        Command::new(exe)
            .args(args)
            .current_dir(root)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .stdin(Stdio::null())
            .stdout(File::create(root.join(format!("{name}.stdout"))).unwrap())
            .stderr(File::create(root.join(format!("{name}.stderr"))).unwrap())
            .spawn()
            .expect("launch owned headless process"),
    )
}

fn run(transport: &str) {
    let client = std::env::var_os("BUBBLEPLAY_CLIENT_EXE")
        .expect("set BUBBLEPLAY_CLIENT_EXE to a built client exe");
    let host = std::env::var_os("BUBBLEPLAY_FAKE_HOST_EXE")
        .expect("set BUBBLEPLAY_FAKE_HOST_EXE to a built fake-wss-host exe");
    let root = Path::new("F:/build/win-client-step6/e2e")
        .join(format!("{transport}-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut host_args: Vec<String> = [
        "--stream",
        "h264",
        "--monitors",
        "--streams-dir",
        "F:/build/spike-streams",
        "--exit-after",
        &HOST_EXIT_S.to_string(),
        "--port",
        &addr.port().to_string(),
        "--stats",
        root.join("host.json").to_str().unwrap(),
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if transport == "webrtc" {
        host_args.push("--webrtc".into());
    }
    let host_started = Instant::now();
    let mut host = launch(Path::new(&host), &host_args, &root, "host");
    let deadline = host_started + Duration::from_secs(HOST_BIND_BUDGET_S);
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
            break;
        }
        assert!(
            host.0.try_wait().unwrap().is_none(),
            "fake host exited before bind"
        );
        assert!(Instant::now() < deadline, "fake host did not bind loopback");
        thread::sleep(Duration::from_millis(25));
    }
    let host_bound_s = host_started.elapsed().as_secs_f64();
    let args = [
        "--dev-wss".to_owned(),
        format!("ws://{addr}"),
        "--dev-headless".into(),
        "--dev-session-script".into(),
        "--transport".into(),
        transport.into(),
        "--codec".into(),
        "h264".into(),
        "--exit-after".into(),
        CLIENT_EXIT_S.to_string(),
        "--stats-out".into(),
        root.join("client.json").to_str().unwrap().into(),
    ];
    let client_started = Instant::now();
    let mut client = launch(Path::new(&client), &args, &root, "client");
    let client_status = client.wait(Duration::from_secs(CLIENT_DEADLINE_S));
    let client_wall_s = client_started.elapsed().as_secs_f64();
    let host_alive_at_client_exit = host.0.try_wait().unwrap().is_none();
    let host_status = host.wait(
        Duration::from_secs(HOST_DEADLINE_S).saturating_sub(host_started.elapsed()),
    );
    let timing = json!({
        "budgets_s": {
            "host_bind": HOST_BIND_BUDGET_S,
            "client_setup": CLIENT_SETUP_BUDGET_S,
            "establish": ESTABLISH_BUDGET_S,
            "exercise": EXERCISE_S,
            "teardown": TEARDOWN_BUDGET_S,
            "host_margin": HOST_MARGIN_S,
            "client_exit_after": CLIENT_EXIT_S,
            "client_deadline": CLIENT_DEADLINE_S,
            "host_exit_after": HOST_EXIT_S,
            "host_deadline": HOST_DEADLINE_S,
        },
        "host_bound_s": host_bound_s,
        "client_wall_s": client_wall_s,
        "host_alive_at_client_exit": host_alive_at_client_exit,
        "host_wall_s": host_started.elapsed().as_secs_f64(),
    });
    fs::write(
        root.join("harness-timing.json"),
        serde_json::to_vec_pretty(&timing).unwrap(),
    )
    .unwrap();
    let doc: Value = serde_json::from_slice(&fs::read(root.join("client.json")).unwrap()).unwrap();
    assert!(client_status.success(), "client failed: {doc}");
    assert!(
        host_status.success(),
        "host failed; inspect {}",
        root.display()
    );
    let host_doc: Value =
        serde_json::from_slice(&fs::read(root.join("host.json")).unwrap()).unwrap();
    assert!(
        host_alive_at_client_exit,
        "host must outlive the complete client run: {timing}"
    );
    assert_eq!(host_doc["epochs"]["issued"], 1);
    assert_eq!(host_doc["monitor"]["switches"], 1);
    assert_eq!(host_doc["monitor"]["switch_failures"], 0);
    assert_eq!(host_doc["stream_mode"]["changes"], 2);
    assert_eq!(host_doc["stream_mode"]["current"], "work");
    assert_eq!(doc["outcome"], "decoded");
    assert_eq!(doc["pass"], true);
    assert_eq!(doc["session_controls"]["complete"], true);
    assert_eq!(doc["session_controls"]["gate_reopened"], true);
    assert!(
        doc["session_controls"]["gate_closes_at_reopen"]
            .as_u64()
            .unwrap()
            > doc["session_controls"]["gate_closes_before"]
                .as_u64()
                .unwrap(),
        "the input gate must really close before reopening"
    );
    assert_eq!(
        doc["session_controls"]["modes"],
        serde_json::json!(["game", "work"])
    );
    assert_eq!(
        doc["transport"]["attempts"], 1,
        "switch must stay in the same signaling epoch"
    );
    assert_eq!(doc["transport"]["established"], 1);
    assert_eq!(doc["transport"]["local_epoch"], 1);
    assert!(doc["video"]["decoded"].as_u64().unwrap() > 2);
    if transport == "webrtc" {
        assert_eq!(host_doc["webrtc"]["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(doc["webrtc"]["selected_transport"], "webrtc");
        assert_eq!(doc["webrtc"]["fallbacks"].as_array().unwrap().len(), 0);
    }
    eprintln!(
        "step-6 {transport}: decoded monitor switch, gate reopened, Game/Work; {}",
        root.display()
    );
}

#[test]
#[ignore = "requires built headless binaries and recorded H.264 on F:"]
fn live_monitor_and_mode_wss() {
    run("wss");
}

#[test]
#[ignore = "requires built headless binaries and recorded H.264 on F:"]
fn live_monitor_and_mode_rtc() {
    run("webrtc");
}
