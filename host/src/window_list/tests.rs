use super::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../../fixtures/window-list-contract.json")).unwrap()
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rect::new(x, y, w, h)
}

fn monitor(selector: &str, x: i32, width: u32, height: u32) -> MonitorInfo {
    MonitorInfo {
        selector: selector.to_owned(),
        name: selector.to_owned(),
        width,
        height,
        origin_x: x,
        origin_y: 0,
        selected: false,
    }
}

fn monitors() -> Vec<MonitorInfo> {
    vec![
        monitor(r"\\.\DISPLAY1", 0, 2560, 1440),
        monitor(r"\\.\DISPLAY2", 2560, 1920, 1080),
    ]
}

fn app(id: u32, title: &str, r: Rect) -> RawWindow {
    RawWindow {
        id,
        thread: id + 1,
        pid: id + 2,
        class: "AppClass".into(),
        exe: "app.exe".into(),
        title: title.into(),
        visible: true,
        rect: r,
        ..RawWindow::default()
    }
}

/// A popup owned by `owner`'s UI thread, like a menu.
fn menu(id: u32, owner: &RawWindow, r: Rect) -> RawWindow {
    RawWindow {
        id,
        thread: owner.thread,
        pid: owner.pid,
        class: "#32768".into(),
        visible: true,
        tool_window: true,
        rect: r,
        ..RawWindow::default()
    }
}

#[test]
fn constants_match_the_fixture() {
    let f = fixture();
    assert_eq!(f["capability"], CAPABILITY);
    assert_eq!(f["wireVersion"], WIRE_VERSION);
    let limits = &f["limits"];
    assert_eq!(limits["maxWindows"], MAX_WINDOWS as u64);
    assert_eq!(limits["maxTitleBytes"], MAX_TITLE_BYTES as u64);
    assert_eq!(limits["maxProcessBytes"], MAX_PROCESS_BYTES as u64);
    assert_eq!(limits["iconSize"], ICON_SIZE);
    assert_eq!(limits["maxIconBytes"], MAX_ICON_BYTES as u64);
    assert_eq!(
        limits["minSendIntervalMs"],
        MIN_SEND_INTERVAL.as_millis() as u64
    );
    assert_eq!(limits["maxListMessageBytes"], MAX_LIST_MESSAGE_BYTES as u64);
    let states: Vec<&str> = [State::Foreground, State::Normal, State::Minimized]
        .iter()
        .map(|s| s.as_str())
        .collect();
    assert_eq!(f["states"], json!(states));
    let reasons: Vec<&str> = [
        FocusFailure::Gone,
        FocusFailure::Elevated,
        FocusFailure::Refused,
    ]
    .iter()
    .map(|r| r.as_str())
    .collect();
    assert_eq!(f["focusFailureReasons"], json!(reasons));
}

#[test]
fn client_messages_parse_as_in_the_fixture() {
    let f = fixture();
    for case in f["clientMessages"]["accepted"].as_array().unwrap() {
        let parsed = parse_client(case["text"].as_str().unwrap());
        let expected = match case["kind"].as_str().unwrap() {
            "focus" => ClientControl::Focus {
                id: case["id"].as_u64().unwrap() as u32,
            },
            "icon" => ClientControl::Icon {
                hash: case["icon"].as_str().unwrap().to_owned(),
            },
            "start" => ClientControl::Start,
            "stop" => ClientControl::Stop,
            other => panic!("unknown kind {other}"),
        };
        assert_eq!(parsed, Some(expected), "{case}");
    }
    for case in f["clientMessages"]["rejected"].as_array().unwrap() {
        assert_eq!(parse_client(case["text"].as_str().unwrap()), None, "{case}");
    }
    for text in f["clientMessages"]["notWindowList"].as_array().unwrap() {
        assert_eq!(parse_client(text.as_str().unwrap()), None, "{text}");
    }
}

#[test]
fn host_messages_match_the_fixture() {
    let f = fixture();
    let entries = vec![
        Entry {
            id: 265142,
            title: "Report.xlsx - Excel".into(),
            process: "EXCEL.EXE".into(),
            icon: Some("9f2c4e6a1b3d5f70".into()),
            rect: rect(120, 80, 1480, 820),
            monitor: Some(r"\\.\DISPLAY1".into()),
            state: State::Foreground,
        },
        Entry {
            id: 131330,
            title: "Notes".into(),
            process: "notepad.exe".into(),
            icon: None,
            rect: rect(2700, 200, 800, 600),
            monitor: Some(r"\\.\DISPLAY2".into()),
            state: State::Normal,
        },
        Entry {
            id: 66012,
            title: "Calculator".into(),
            process: "CalculatorApp.exe".into(),
            icon: None,
            rect: rect(300, 300, 320, 500),
            monitor: Some(r"\\.\DISPLAY1".into()),
            state: State::Minimized,
        },
    ];
    let parse = |text: &str| serde_json::from_str::<Value>(text).unwrap();
    let hosts = &f["hostMessages"];
    assert_eq!(
        parse(&list_message(3, &entries)),
        parse(hosts["windowList"]["text"].as_str().unwrap())
    );
    assert_eq!(
        parse(&focus_failed_message(265142, FocusFailure::Elevated)),
        parse(hosts["focusFailed"]["text"].as_str().unwrap())
    );
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgo=")
        .unwrap();
    assert_eq!(
        parse(&icon_message("9f2c4e6a1b3d5f70", &png)),
        parse(hosts["icon"]["text"].as_str().unwrap())
    );
}

#[test]
fn only_app_windows_are_switchable() {
    let plain = app(10, "Editor", rect(0, 0, 800, 600));
    assert!(is_switchable(&plain));
    let owned = RawWindow {
        owner: 10,
        ..plain.clone()
    };
    assert!(
        !is_switchable(&owned),
        "an owned dialog is not its own entry"
    );
    assert!(is_switchable(&RawWindow {
        app_window: true,
        ..owned
    }));
    assert!(!is_switchable(&RawWindow {
        tool_window: true,
        ..plain.clone()
    }));
    assert!(!is_switchable(&RawWindow {
        no_activate: true,
        ..plain.clone()
    }));
    assert!(!is_switchable(&RawWindow {
        visible: false,
        ..plain.clone()
    }));
    assert!(!is_switchable(&RawWindow {
        cloaked: true,
        ..plain.clone()
    }));
    assert!(!is_switchable(&RawWindow {
        title: "  ".into(),
        ..plain.clone()
    }));
    assert!(!is_switchable(&RawWindow {
        class: "Shell_TrayWnd".into(),
        ..plain.clone()
    }));
    assert!(!is_switchable(&RawWindow {
        class: "Ghost".into(),
        ..plain.clone()
    }));
    assert!(!is_switchable(&RawWindow {
        exe: "SnippingTool.exe".into(),
        ..plain
    }));
}

#[test]
fn the_crop_takes_in_menus_and_submenus_above_the_app() {
    let editor = app(10, "Editor", rect(100, 100, 800, 600));
    // A dropdown hanging below the window, a submenu touching only the dropdown, and an
    // unrelated window of another process overlapping the app.
    let dropdown = menu(20, &editor, rect(150, 690, 200, 300));
    let submenu = menu(21, &editor, rect(350, 900, 200, 150));
    let other = app(30, "Other", rect(500, 500, 900, 900));
    let windows = vec![submenu, dropdown, other, editor];
    assert_eq!(
        crop_rect(3, &windows),
        Rect {
            left: 100,
            top: 100,
            right: 900,
            bottom: 1050
        }
    );
}

#[test]
fn the_crop_leaves_out_what_is_below_far_away_or_a_sibling_window() {
    let browser = app(10, "Browser", rect(0, 0, 1000, 800));
    // A second browser window on the same UI thread, above and overlapping.
    let sibling = RawWindow {
        id: 11,
        title: "Browser 2".into(),
        rect: rect(600, 600, 900, 700),
        ..browser.clone()
    };
    // An owned dialog far from the window, and a menu below the app in z-order.
    let far = RawWindow {
        owner: 10,
        ..menu(12, &browser, rect(3000, 0, 300, 200))
    };
    let below = menu(13, &browser, rect(990, 10, 300, 300));
    let windows = vec![sibling, far, browser, below];
    assert_eq!(crop_rect(2, &windows), rect(0, 0, 1000, 800));
}

#[test]
fn an_owned_dialog_counts_even_on_another_thread() {
    let editor = app(10, "Editor", rect(0, 0, 800, 600));
    let dialog = RawWindow {
        owner: 10,
        thread: 99,
        ..menu(20, &editor, rect(700, 500, 400, 300))
    };
    let windows = vec![dialog, editor];
    assert_eq!(crop_rect(1, &windows), rect(0, 0, 1100, 800));
}

#[test]
fn build_keeps_z_order_and_sets_states_and_monitors() {
    let top = app(1, "Top", rect(2700, 100, 600, 400));
    let back = RawWindow {
        minimized: true,
        ..app(2, "Back", rect(100, 100, 300, 300))
    };
    let off = app(3, "Off screen", rect(-5000, -5000, 100, 100));
    let entries = build(&[top, back, off], 1, &monitors());
    let summary: Vec<(u32, State, Option<&str>)> = entries
        .iter()
        .map(|e| (e.id, e.state, e.monitor.as_deref()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (1, State::Foreground, Some(r"\\.\DISPLAY2")),
            (2, State::Minimized, Some(r"\\.\DISPLAY1")),
            (3, State::Normal, None),
        ]
    );
}

#[test]
fn monitor_is_the_one_holding_most_of_the_window() {
    // 600 px on DISPLAY1, 200 px on DISPLAY2.
    assert_eq!(
        monitor_for(rect(1960, 0, 800, 400), &monitors()).as_deref(),
        Some(r"\\.\DISPLAY1")
    );
}

#[test]
fn titles_are_cut_on_a_character_boundary_and_lists_are_capped() {
    assert_eq!(truncate("aé", 2), "a");
    assert_eq!(truncate("abc", 8), "abc");
    let long = "é".repeat(200);
    let windows: Vec<RawWindow> = (0..80)
        .map(|i| app(i * 10 + 10, &long, rect(0, 0, 100, 100)))
        .collect();
    let entries = build(&windows, 0, &monitors());
    assert_eq!(entries.len(), MAX_WINDOWS);
    assert!(entries
        .iter()
        .all(|e| e.title.len() <= MAX_TITLE_BYTES && e.title.len() % 2 == 0));
}

#[test]
fn an_oversized_list_drops_trailing_windows() {
    let entry = Entry {
        id: 1,
        title: "x".repeat(MAX_TITLE_BYTES),
        process: "p".repeat(MAX_PROCESS_BYTES),
        icon: Some("0123456789abcdef".into()),
        rect: rect(0, 0, 1, 1),
        monitor: Some(r"\\.\DISPLAY1".into()),
        state: State::Normal,
    };
    let entries: Vec<Entry> = std::iter::repeat_n(entry, 300).collect();
    let text = list_message(1, &entries);
    assert!(text.len() <= MAX_LIST_MESSAGE_BYTES);
    let windows = serde_json::from_str::<Value>(&text).unwrap()["windows"]
        .as_array()
        .unwrap()
        .len();
    assert!(windows > 0 && windows < 300);
}

#[test]
fn the_sender_sends_changes_only_in_focus_mode_and_at_its_pace() {
    let entries = |title: &str| build(&[app(1, title, rect(0, 0, 10, 10))], 1, &monitors());
    let t0 = Instant::now();
    let mut sender = Sender::default();
    assert_eq!(
        sender.next(t0, entries("A")),
        None,
        "off until focus mode starts"
    );
    sender.start();
    let first = sender.next(t0, entries("A")).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&first).unwrap()["seq"], 1);
    assert!(sender.listed(1) && !sender.listed(2));
    assert_eq!(
        sender.next(t0 + MIN_SEND_INTERVAL, entries("A")),
        None,
        "unchanged"
    );
    assert_eq!(
        sender.next(t0 + Duration::from_millis(100), entries("B")),
        None,
        "too soon"
    );
    let second = sender.next(t0 + MIN_SEND_INTERVAL, entries("B")).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&second).unwrap()["seq"], 2);
    sender.stop();
    assert!(!sender.active() && !sender.listed(1));
    assert_eq!(sender.next(t0 + MIN_SEND_INTERVAL * 4, entries("C")), None);
}

#[test]
fn icons_are_hashed_and_bounded() {
    assert_eq!(icon_hash(b""), "cbf29ce484222325");
    let mut cache = IconCache::default();
    let hash = cache.insert(vec![1, 2, 3]).unwrap();
    assert!(valid_hash(&hash));
    assert_eq!(cache.get(&hash), Some(&[1u8, 2, 3][..]));
    assert_eq!(cache.insert(vec![0; MAX_ICON_BYTES + 1]), None);
    assert_eq!(cache.insert(Vec::new()), None);
}

#[test]
fn icons_keep_their_transparency() {
    // BGRA: an opaque red pixel, a fully transparent one, and a half-transparent white one.
    let black = [0, 0, 255, 255, 0, 0, 0, 255, 128, 128, 128, 255];
    let white = [0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255];
    assert_eq!(
        unblend(&black, &white),
        vec![255, 0, 0, 255, 0, 0, 0, 0, 255, 255, 255, 128]
    );
}
