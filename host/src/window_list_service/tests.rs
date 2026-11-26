use super::*;
use crate::follow_window::Rect;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Log {
    focused: Vec<u32>,
    samples: usize,
}

struct Fake {
    log: Arc<Mutex<Log>>,
    title: Arc<Mutex<String>>,
}

impl Backend for Fake {
    fn sample(&mut self) -> Snapshot {
        self.log.lock().unwrap().samples += 1;
        let window = RawWindow {
            id: 7,
            thread: 1,
            pid: 2,
            class: "App".into(),
            exe: "app.exe".into(),
            title: self.title.lock().unwrap().clone(),
            visible: true,
            rect: Rect::new(10, 10, 300, 200),
            ..RawWindow::default()
        };
        Snapshot {
            windows: vec![window],
            foreground: 7,
            monitors: vec![MonitorInfo {
                selector: r"\\.\DISPLAY1".into(),
                name: "Monitor 1".into(),
                width: 1920,
                height: 1080,
                origin_x: 0,
                origin_y: 0,
                selected: true,
            }],
        }
    }

    fn focus(&mut self, id: u32) -> Result<Option<String>, FocusFailure> {
        self.log.lock().unwrap().focused.push(id);
        if id == 7 {
            Ok(Some(r"\\.\DISPLAY2".into()))
        } else {
            Err(FocusFailure::Refused)
        }
    }

    fn icon(&mut self, hash: &str) -> Option<Vec<u8>> {
        (hash == "0123456789abcdef").then(|| vec![1, 2, 3])
    }
}

fn session() -> (Session, Arc<Mutex<Log>>, Arc<Mutex<String>>) {
    let log = Arc::new(Mutex::new(Log::default()));
    let title = Arc::new(Mutex::new("First".to_owned()));
    let fake = Fake {
        log: Arc::clone(&log),
        title: Arc::clone(&title),
    };
    (Session::start(fake), log, title)
}

/// Ticks until one produces something, or two seconds pass.
fn next_tick(session: &mut Session) -> Tick {
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        let tick = session.tick(Instant::now());
        if tick != Tick::default() || Instant::now() > end {
            return tick;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn kind(message: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(message).unwrap();
    value["type"].as_str().unwrap().to_owned()
}

#[test]
fn nothing_is_sampled_or_sent_before_the_switcher_opens() {
    let (mut session, log, _) = session();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(session.tick(Instant::now()), Tick::default());
    assert_eq!(log.lock().unwrap().samples, 0);
}

#[test]
fn start_sends_a_list_and_then_each_change() {
    let (mut session, _, title) = session();
    assert!(session.on_control(ClientControl::Start).is_empty());
    let first = next_tick(&mut session);
    assert_eq!(first.messages.len(), 1);
    assert_eq!(kind(&first.messages[0]), "window-list");
    assert!(first.messages[0].contains("First"));
    *title.lock().unwrap() = "Second".into();
    let second = next_tick(&mut session);
    assert!(second.messages[0].contains("Second"));
}

#[test]
fn focusing_a_listed_window_reports_its_monitor() {
    let (mut session, log, _) = session();
    session.on_control(ClientControl::Start);
    next_tick(&mut session);
    assert!(session
        .on_control(ClientControl::Focus { id: 7 })
        .is_empty());
    let end = Instant::now() + Duration::from_secs(2);
    let mut switched = None;
    while switched.is_none() && Instant::now() < end {
        switched = session.tick(Instant::now()).switch_to;
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(switched.as_deref(), Some(r"\\.\DISPLAY2"));
    assert_eq!(log.lock().unwrap().focused, vec![7]);
}

#[test]
fn a_window_the_client_was_never_shown_is_gone() {
    let (mut session, log, _) = session();
    let messages = session.on_control(ClientControl::Focus { id: 99 });
    assert_eq!(messages.len(), 1);
    let value: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
    assert_eq!(
        (value["id"].as_u64(), value["reason"].as_str()),
        (Some(99), Some("gone"))
    );
    drop(session);
    assert!(log.lock().unwrap().focused.is_empty());
}

#[test]
fn icons_are_answered_and_stop_ends_the_lists() {
    let (mut session, log, _) = session();
    session.on_control(ClientControl::Icon {
        hash: "0123456789abcdef".into(),
    });
    let icon = next_tick(&mut session);
    assert_eq!(kind(&icon.messages[0]), "window-icon");
    session.on_control(ClientControl::Start);
    next_tick(&mut session);
    session.on_control(ClientControl::Stop);
    std::thread::sleep(SAMPLE_INTERVAL * 2);
    let _ = session.tick(Instant::now());
    let samples = log.lock().unwrap().samples;
    std::thread::sleep(SAMPLE_INTERVAL * 2);
    assert_eq!(session.tick(Instant::now()), Tick::default());
    assert_eq!(
        log.lock().unwrap().samples,
        samples,
        "no sampling after stop"
    );
}
