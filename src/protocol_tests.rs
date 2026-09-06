use super::*;

/// Every command survives encode→frame-payload→decode unchanged, including
/// the `Watch{None}` null, raw `Input` bytes (0 and 255), and the no-field
/// `Shutdown`.
///
/// `variant_index` exhaustively matches `Command`, and `seen` verifies that
/// `cases` covers every arm. This guards `decode_command`, whose unknown-tag
/// fallback prevents the compiler from detecting an omitted decode arm.
#[test]
fn command_round_trips() {
    let cases = [
        Command::Spawn {
            command: "echo hi".into(),
            cwd: PathBuf::from("/tmp"),
            group: None,
        },
        Command::Spawn {
            // Exercise byte-preserving serialization of a non-UTF-8 path.
            command: "ls".into(),
            cwd: PathBuf::from(OsString::from_vec(b"/tmp/\xff\xfe dir".to_vec())),
            group: None,
        },
        Command::Spawn {
            command: "make".into(),
            cwd: PathBuf::from("/tmp"),
            group: Some("build".into()),
        },
        Command::Kill { id: 7 },
        Command::Remove { id: 3 },
        Command::Restart { id: 4 },
        Command::Tag { id: 2, on: true },
        Command::SetGroup {
            id: 2,
            group: Some("infra".into()),
        },
        Command::SetGroup { id: 2, group: None },
        Command::SetName {
            id: 2,
            name: Some("api server".into()),
        },
        Command::SetName { id: 2, name: None },
        Command::Resize {
            rows: 30,
            cols: 100,
        },
        Command::Watch {
            id: Some(5),
            attached: true,
        },
        Command::Watch {
            id: Some(5),
            attached: false,
        },
        Command::Watch {
            id: None,
            attached: false,
        },
        Command::Input {
            id: 1,
            bytes: vec![0, 27, 91, 255],
        },
        Command::Paste {
            // Non-UTF-8 and marker-shaped bytes must survive: the core, not
            // the client, decides what the child receives.
            id: 6,
            bytes: b"line1\nline2\x1b[201~\xff".to_vec(),
        },
        Command::Mouse {
            id: 8,
            kind: MouseKind::WheelDown,
            col: 79,
            row: 23,
        },
        Command::Mouse {
            id: 8,
            kind: MouseKind::Press(MouseBtn::Left),
            col: 0,
            row: 0,
        },
        Command::Mouse {
            id: 8,
            kind: MouseKind::Drag(MouseBtn::Middle),
            col: 10,
            row: 5,
        },
        Command::Mouse {
            id: 8,
            kind: MouseKind::Release(MouseBtn::Right),
            col: 10,
            row: 5,
        },
        Command::Key {
            id: 9,
            code: Key::Char('λ'),
            mods: Mods::default(),
        },
        Command::Key {
            id: 9,
            code: Key::F(7),
            mods: Mods {
                ctrl: true,
                ..Mods::default()
            },
        },
        Command::Key {
            id: 9,
            // A modified arrow carries all three bits through the wire.
            code: Key::Left,
            mods: Mods {
                shift: true,
                alt: true,
                ctrl: true,
            },
        },
        Command::Key {
            id: 9,
            code: Key::Enter,
            mods: Mods::default(),
        },
        Command::Scrollback {
            id: 3,
            action: ScrollAction::Up(23),
        },
        Command::Scrollback {
            id: 3,
            action: ScrollAction::Down(1),
        },
        Command::Scrollback {
            id: 3,
            action: ScrollAction::Top,
        },
        Command::Scrollback {
            id: 3,
            action: ScrollAction::Live,
        },
        Command::SaveSession {
            name: "work".into(),
        },
        Command::LoadSession {
            name: "home".into(),
        },
        Command::LoadRecovery {
            stem: "20260714-093015-4242".into(),
        },
        Command::ListSessions,
        Command::Shutdown,
    ];
    // Keep this match exhaustive: `seen` then proves that `cases` covers every
    // arm.
    fn variant_index(c: &Command) -> usize {
        match c {
            Command::Spawn { .. } => 0,
            Command::Kill { .. } => 1,
            Command::Remove { .. } => 2,
            Command::Restart { .. } => 3,
            Command::Tag { .. } => 4,
            Command::SetGroup { .. } => 5,
            Command::SetName { .. } => 6,
            Command::Resize { .. } => 7,
            Command::Watch { .. } => 8,
            Command::Input { .. } => 9,
            Command::Paste { .. } => 10,
            Command::Mouse { .. } => 11,
            Command::Key { .. } => 12,
            Command::Scrollback { .. } => 13,
            Command::SaveSession { .. } => 14,
            Command::LoadSession { .. } => 15,
            Command::LoadRecovery { .. } => 16,
            Command::ListSessions => 17,
            Command::Shutdown => 18,
        }
    }
    let mut seen = [false; 19];
    for c in cases {
        seen[variant_index(&c)] = true;
        let (k, p) = encode_command(&c);
        assert_eq!(decode_command(k, &p).as_ref(), Some(&c), "round-trip {c:?}");
    }
    for (i, covered) in seen.iter().enumerate() {
        assert!(
            covered,
            "Command variant #{i} (see variant_index) never round-tripped: \
             add a `cases` entry above and its decode arm in decode_command"
        );
    }
}

#[test]
fn hello_ok_round_trips() {
    let ack = Event::HelloOk;
    let (k, p) = encode_event(&ack);
    assert_eq!(k, KIND_CONTROL);
    assert_eq!(decode_event(k, &p), Some(ack));
}

/// Handshake environment entries and the cwd round-trip byte-for-byte,
/// including non-UTF-8 bytes in both.
#[test]
fn hello_round_trips() {
    let ctx = LaunchContext {
        env: vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            (
                OsString::from_vec(b"BAD\xff\xfe".to_vec()),
                OsString::from_vec(b"v\xff".to_vec()),
            ),
        ],
        cwd: PathBuf::from(OsString::from_vec(b"/home/x\xff\xfe".to_vec())),
    };
    let (k, p) = encode_hello(&ctx);
    assert_eq!(k, KIND_HELLO);
    assert_eq!(decode_hello(k, &p), Some((PROTOCOL_VERSION, ctx)));

    let empty = LaunchContext {
        env: Vec::new(),
        cwd: PathBuf::from("/"),
    };
    let (k, p) = encode_hello(&empty);
    assert_eq!(decode_hello(k, &p), Some((PROTOCOL_VERSION, empty)));
}

/// A hello payload is valid only in a hello frame.
#[test]
fn hello_requires_its_own_frame_kind() {
    let (_, p) = encode_hello(&LaunchContext {
        env: Vec::new(),
        cwd: PathBuf::from("/"),
    });
    assert_eq!(decode_hello(KIND_CONTROL, &p), None);
    assert_eq!(decode_command(KIND_CONTROL, &p), None);
}

/// Malformed environment entries reject the entire hello frame.
#[test]
fn hello_with_malformed_env_is_rejected() {
    for env in [
        r#"[["P@TH","L2Jpbg=="]]"#,    // invalid base64 character
        r#"[["QUFBQUE","L2Jpbg=="]]"#, // truncated: missing padding
        r#"[["UEFUSA==","AAAA="]]"#,   // bad padding length
        r#"[[[80],[65]]]"#,            // env pairs must contain base64 strings
        r#"["PATH=/bin"]"#,            // flat string pair
    ] {
        // Keep the cwd valid so each case isolates env validation.
        let json = format!(r#"{{"v":4,"cwd":"Lw==","env":{env}}}"#);
        assert_eq!(
            decode_hello(KIND_HELLO, json.as_bytes()),
            None,
            "should reject env {env}"
        );
    }
}

/// A hello with a non-base64 cwd is rejected.
#[test]
fn hello_with_malformed_cwd_is_rejected() {
    let json = r#"{"v":3,"cwd":"/home/user","env":[]}"#;
    assert_eq!(decode_hello(KIND_HELLO, json.as_bytes()), None);
}

/// Out-of-range numeric fields reject the whole command.
#[test]
fn out_of_range_numerics_are_rejected() {
    for json in [
        r#"{"t":"resize","rows":65536,"cols":100}"#,
        r#"{"t":"resize","rows":30,"cols":65536}"#,
        r#"{"t":"mouse","id":1,"k":"wu","col":65536,"row":0}"#,
        r#"{"t":"mouse","id":1,"k":"wu","col":0,"row":65536}"#,
        r#"{"t":"sb","id":1,"a":"u","n":65536}"#,
        r#"{"t":"sb","id":1,"a":"d","n":-1}"#,
    ] {
        assert_eq!(
            decode_command(KIND_CONTROL, json.as_bytes()),
            None,
            "should reject {json}"
        );
    }
}

/// Invalid base64 and non-string byte or path fields reject the command.
#[test]
fn invalid_base64_is_rejected() {
    for json in [
        r#"{"t":"input","id":1,"bytes":"!!!"}"#,
        r#"{"t":"input","id":1,"bytes":[0,27]}"#, // bytes must be a base64 string
        r#"{"t":"paste","id":1,"bytes":"AAAA="}"#, // bad padding length
        r#"{"t":"spawn","command":"ls","cwd":"/tmp/x"}"#, // plain path
    ] {
        assert_eq!(
            decode_command(KIND_CONTROL, json.as_bytes()),
            None,
            "should reject {json}"
        );
    }
}

/// Build a `KIND_SCREEN` payload with a raw header and an empty byte tail.
fn screen_payload(header: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + header.len());
    p.extend_from_slice(&(header.len() as u32).to_be_bytes());
    p.extend_from_slice(header.as_bytes());
    p
}

/// Build the neutral task view used by exact wire-format assertions.
fn tv(id: u64) -> TaskView {
    TaskView {
        id,
        command: "x".into(),
        cwd: PathBuf::from("/"),
        tagged: false,
        group: None,
        name: None,
        lifecycle: Lifecycle::Ok,
        preview: Preview::floor(String::new()),
        started_ago: Duration::from_millis(0),
        quiet_ago: None,
        finished_ago: None,
    }
}

/// A mistyped member in `lines`, `tasks`, or `names` rejects the whole
/// event, keeping decoded rows aligned with their encoded positions.
#[test]
fn mistyped_event_members_are_rejected() {
    for header in [
        // Numeric member in `lines`.
        r#"{"id":1,"cursor":[0,0],"hide":false,"mouse":false,"alt":false,"ascr":false,"sb":0,"lines":["ok",5]}"#,
        // Out-of-range cursor cell.
        r#"{"id":1,"cursor":[65536,0],"hide":false,"mouse":false,"alt":false,"ascr":false,"sb":0,"lines":[]}"#,
    ] {
        assert_eq!(
            decode_event(KIND_SCREEN, &screen_payload(header)),
            None,
            "should reject header {header}"
        );
    }
    for json in [
        r#"{"t":"tasks","tasks":[{"id":"nope"}]}"#,
        // The cwd must be a base64 string.
        r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"/x","tagged":true,"life":"ok","preview":"","src":"floor","frozen":false,"started_ms":0}]}"#,
        // A present group must be a string; only missing/null means unassigned.
        r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":true,"life":"ok","preview":"","src":"floor","frozen":false,"started_ms":0,"group":5}]}"#,
        // A present name must be a string.
        r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":true,"life":"ok","preview":"","src":"floor","frozen":false,"started_ms":0,"name":5}]}"#,
        r#"{"t":"tasks","tasks":["flat"]}"#,
        // Numeric member in `names`.
        r#"{"t":"sessions","names":["ok",5]}"#,
    ] {
        assert_eq!(
            decode_event(KIND_CONTROL, json.as_bytes()),
            None,
            "should reject {json}"
        );
    }
}

/// Screen events without the required alternate-scroll field are rejected.
#[test]
fn screen_header_without_alt_scroll_is_rejected() {
    let header =
        r#"{"id":1,"cursor":[0,0],"hide":false,"mouse":false,"alt":true,"sb":0,"lines":[]}"#;
    assert_eq!(decode_event(KIND_SCREEN, &screen_payload(header)), None);
}

#[test]
fn tasks_and_status_round_trip() {
    let tasks = Event::Tasks(vec![
        TaskView {
            id: 1,
            command: "vim".into(),
            cwd: PathBuf::from("/home/x"),
            tagged: true,
            group: Some("x".into()),
            name: Some("editor".into()),
            lifecycle: Lifecycle::Idle,
            preview: Preview {
                text: "~ line".into(),
                source: PreviewSource::Title,
                rule: None,
                frozen: false,
            },
            started_ago: Duration::from_millis(4200),
            quiet_ago: Some(Duration::from_millis(700)),
            finished_ago: None,
        },
        TaskView {
            command: "make".into(),
            // Exercise byte-preserving task-path serialization.
            cwd: PathBuf::from(OsString::from_vec(b"/srv/\xff\xfe".to_vec())),
            lifecycle: Lifecycle::Active,
            started_ago: Duration::from_millis(10),
            ..tv(2)
        },
    ]);
    let (k, p) = encode_event(&tasks);
    assert_eq!(k, KIND_CONTROL);
    assert_eq!(decode_event(k, &p), Some(tasks));

    let status = Event::Status("saved 'x'".into());
    let (k, p) = encode_event(&status);
    assert_eq!(decode_event(k, &p), Some(status));
}

/// A spawn acknowledgement round-trips with an id above `u32::MAX`.
#[test]
fn spawned_round_trips() {
    let ev = Event::Spawned {
        id: u64::from(u32::MAX) + 7,
    };
    let (k, p) = encode_event(&ev);
    assert_eq!(k, KIND_CONTROL);
    assert_eq!(decode_event(k, &p), Some(ev));
}

/// `SetGroup` emits `"g"` only for an assignment. A missing or null `"g"`
/// decodes as a clear.
#[test]
fn set_group_wire_form() {
    let (k, p) = encode_command(&Command::SetGroup {
        id: 3,
        group: Some("infra".into()),
    });
    assert_eq!(k, KIND_CONTROL);
    assert_eq!(
        std::str::from_utf8(&p).unwrap(),
        r#"{"t":"group","id":3,"g":"infra"}"#
    );
    let (_, p) = encode_command(&Command::SetGroup { id: 3, group: None });
    assert!(!String::from_utf8(p).unwrap().contains("\"g\""));
    // An explicit null clears, same as an omitted key.
    assert_eq!(
        decode_command(KIND_CONTROL, br#"{"t":"group","id":3,"g":null}"#),
        Some(Command::SetGroup { id: 3, group: None })
    );
    // A present group must be a string.
    assert_eq!(
        decode_command(KIND_CONTROL, br#"{"t":"group","id":3,"g":5}"#),
        None
    );
}

/// `SetName` omits `"n"` when clearing; a missing or null `"n"` decodes as
/// a clear.
#[test]
fn set_name_wire_form() {
    let (k, p) = encode_command(&Command::SetName {
        id: 3,
        name: Some("api".into()),
    });
    assert_eq!(k, KIND_CONTROL);
    assert_eq!(
        std::str::from_utf8(&p).unwrap(),
        r#"{"t":"name","id":3,"n":"api"}"#
    );
    let (_, p) = encode_command(&Command::SetName { id: 3, name: None });
    assert!(!String::from_utf8(p).unwrap().contains("\"n\""));
    // An explicit null clears, same as an omitted key.
    assert_eq!(
        decode_command(KIND_CONTROL, br#"{"t":"name","id":3,"n":null}"#),
        Some(Command::SetName { id: 3, name: None })
    );
    // A present name must be a string.
    assert_eq!(
        decode_command(KIND_CONTROL, br#"{"t":"name","id":3,"n":5}"#),
        None
    );
}

/// Task frames omit `"group"` when unassigned; an absent key decodes as
/// `None`.
#[test]
fn tasks_frame_group_key_is_optional() {
    // "Lw==" is the base64 encoding of "/".
    let ungrouped = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"life":"ok","preview":"","src":"floor","frozen":false,"started_ms":0}]}"#;
    match decode_event(KIND_CONTROL, ungrouped.as_bytes()) {
        Some(Event::Tasks(v)) => assert_eq!(v[0].group, None),
        other => panic!("expected tasks event, got {other:?}"),
    }
    // Encoding an unassigned task omits the group key.
    let (_, p) = encode_event(&Event::Tasks(vec![tv(1)]));
    assert_eq!(std::str::from_utf8(&p).unwrap(), ungrouped);

    let grouped = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"group":"infra","life":"ok","preview":"","src":"floor","frozen":false,"started_ms":0}]}"#;
    match decode_event(KIND_CONTROL, grouped.as_bytes()) {
        Some(Event::Tasks(v)) => assert_eq!(v[0].group.as_deref(), Some("infra")),
        other => panic!("expected tasks event, got {other:?}"),
    }
}

/// Task frames omit `"name"` when unnamed; an absent key decodes as
/// `None`.
#[test]
fn tasks_frame_name_key_is_optional() {
    // "Lw==" is the base64 encoding of "/".
    let unnamed = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"life":"ok","preview":"","src":"floor","frozen":false,"started_ms":0}]}"#;
    match decode_event(KIND_CONTROL, unnamed.as_bytes()) {
        Some(Event::Tasks(v)) => assert_eq!(v[0].name, None),
        other => panic!("expected tasks event, got {other:?}"),
    }
    // Encoding an unnamed task omits the name key.
    let (_, p) = encode_event(&Event::Tasks(vec![tv(1)]));
    assert_eq!(std::str::from_utf8(&p).unwrap(), unnamed);

    let named = r#"{"t":"tasks","tasks":[{"id":1,"command":"x","cwd":"Lw==","tagged":false,"name":"build","life":"ok","preview":"","src":"floor","frozen":false,"started_ms":0}]}"#;
    match decode_event(KIND_CONTROL, named.as_bytes()) {
        Some(Event::Tasks(v)) => assert_eq!(v[0].name.as_deref(), Some("build")),
        other => panic!("expected tasks event, got {other:?}"),
    }
}

/// Each lifecycle preserves its optional age and omits the inapplicable age.
#[test]
fn lifecycle_and_age_fields_round_trip() {
    for lifecycle in [
        Lifecycle::Active,
        Lifecycle::Idle,
        Lifecycle::Ok,
        Lifecycle::Failed,
    ] {
        for age in [None, Some(Duration::from_millis(12_000))] {
            let live = matches!(lifecycle, Lifecycle::Active | Lifecycle::Idle);
            let tasks = Event::Tasks(vec![TaskView {
                lifecycle,
                started_ago: Duration::from_millis(60_000),
                quiet_ago: if live { age } else { None },
                finished_ago: if live { None } else { age },
                ..tv(1)
            }]);
            let (k, p) = encode_event(&tasks);
            let s = std::str::from_utf8(&p).unwrap();
            assert!(!s.contains("\"parked\""), "frame was {s}");
            assert_eq!(
                s.contains("\"quiet_ms\""),
                live && age.is_some(),
                "frame was {s}"
            );
            assert_eq!(
                s.contains("\"finished_ms\""),
                !live && age.is_some(),
                "frame was {s}"
            );
            assert_eq!(decode_event(k, &p), Some(tasks));
        }
    }
}

/// Null ages remain unknown, just like omitted ages.
#[test]
fn tasks_frame_null_ages_decode_as_unknown() {
    let tasks = Event::Tasks(vec![tv(1)]);
    let (k, p) = encode_event(&tasks);
    let s = String::from_utf8(p).unwrap().replace(
        "\"started_ms\":0",
        "\"started_ms\":0,\"quiet_ms\":null,\"finished_ms\":null",
    );
    assert_eq!(decode_event(k, s.as_bytes()), Some(tasks));
}

/// Every preview source round-trips with its frozen flag, and `rule`
/// never crosses the wire: an encoded `Some` decodes as `None`.
#[test]
fn preview_source_and_frozen_round_trip() {
    let base = TaskView {
        lifecycle: Lifecycle::Active,
        preview: Preview::floor("p".into()),
        quiet_ago: Some(Duration::from_millis(1)),
        ..tv(1)
    };
    let tasks = Event::Tasks(vec![
        base.clone(),
        TaskView {
            id: 2,
            preview: Preview {
                source: PreviewSource::Marker,
                ..base.preview.clone()
            },
            ..base.clone()
        },
        TaskView {
            id: 3,
            preview: Preview {
                source: PreviewSource::Title,
                frozen: true,
                ..base.preview.clone()
            },
            ..base.clone()
        },
        TaskView {
            id: 4,
            preview: Preview {
                source: PreviewSource::Anchor,
                ..base.preview.clone()
            },
            ..base.clone()
        },
    ]);
    let (k, p) = encode_event(&tasks);
    assert_eq!(decode_event(k, &p), Some(tasks));

    // Encoding omits the process-local matcher rule.
    let ruled = Event::Tasks(vec![TaskView {
        preview: Preview {
            source: PreviewSource::Anchor,
            rule: Some("claude-status"),
            ..base.preview.clone()
        },
        ..base.clone()
    }]);
    let (k, p) = encode_event(&ruled);
    assert!(
        !String::from_utf8(p.clone())
            .unwrap()
            .contains("claude-status")
    );
    match decode_event(k, &p) {
        Some(Event::Tasks(v)) => {
            assert_eq!(v[0].preview.source, PreviewSource::Anchor);
            assert_eq!(v[0].preview.rule, None, "rule must stay daemon-side");
        }
        other => panic!("expected tasks event, got {other:?}"),
    }
}

/// Same-version task frames require a known source and a boolean frozen flag.
#[test]
fn tasks_frame_requires_valid_preview_metadata() {
    let tasks = Event::Tasks(vec![tv(1)]);
    let (k, p) = encode_event(&tasks);
    assert_eq!(decode_event(k, &p), Some(tasks));
    let valid = String::from_utf8(p).unwrap();
    for (field, value, invalid) in [
        (
            "src",
            "\"floor\"",
            &["null", "false", "0", "[]", "{}", "\"vibes\""][..],
        ),
        (
            "frozen",
            "false",
            &["null", "0", "\"false\"", "[]", "{}"][..],
        ),
    ] {
        let member = format!("\"{field}\":{value}");
        let missing = valid.replace(&format!("{member},"), "");
        assert_eq!(decode_event(k, missing.as_bytes()), None, "missing {field}");
        for value in invalid {
            let bad = valid.replace(&member, &format!("\"{field}\":{value}"));
            assert_eq!(decode_event(k, bad.as_bytes()), None, "frame was {bad}");
        }
    }
}

/// `Sessions` carries the picker's names verbatim: several names, an empty
/// list, and names with spaces and non-ASCII all round-trip.
#[test]
fn sessions_event_round_trips() {
    for names in [
        vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
        Vec::new(),
        vec!["my session".to_string(), "café ☕".to_string()],
    ] {
        let ev = Event::Sessions {
            names,
            recovery: Vec::new(),
        };
        let (k, p) = encode_event(&ev);
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(decode_event(k, &p).as_ref(), Some(&ev), "round-trip {ev:?}");
    }
}

/// Recovery entries round-trip with their exact wire fields.
#[test]
fn sessions_recovery_entries_round_trip_and_pin_the_wire_shape() {
    let ev = Event::Sessions {
        names: vec!["work".to_string()],
        recovery: vec![
            RecoveryEntry {
                stem: "20260715-070000-22".into(),
                label: "autosaved 2026-07-15 07:00".into(),
                tasks: 3,
                age_secs: 42,
            },
            RecoveryEntry {
                stem: "20260714-093015-11".into(),
                label: "autosaved 2026-07-14 09:30".into(),
                tasks: 1,
                age_secs: 90_000,
            },
        ],
    };
    let (k, p) = encode_event(&ev);
    assert_eq!(k, KIND_CONTROL);
    assert_eq!(
        std::str::from_utf8(&p).unwrap(),
        r#"{"t":"sessions","names":["work"],"recovery":[{"stem":"20260715-070000-22","label":"autosaved 2026-07-15 07:00","tasks":3,"age":42},{"stem":"20260714-093015-11","label":"autosaved 2026-07-14 09:30","tasks":1,"age":90000}]}"#
    );
    assert_eq!(decode_event(k, &p), Some(ev));
}

/// A missing or non-array `recovery` value decodes as an empty list.
#[test]
fn sessions_frame_without_recovery_key_decodes_empty() {
    for json in [
        r#"{"t":"sessions","names":["a"]}"#,
        r#"{"t":"sessions","names":["a"],"recovery":null}"#,
        r#"{"t":"sessions","names":["a"],"recovery":"junk"}"#,
    ] {
        assert_eq!(
            decode_event(KIND_CONTROL, json.as_bytes()),
            Some(Event::Sessions {
                names: vec!["a".to_string()],
                recovery: Vec::new(),
            }),
            "should tolerate {json}"
        );
    }
}

/// Malformed recovery members are skipped without dropping valid entries.
#[test]
fn malformed_recovery_members_drop_without_rejecting_the_event() {
    let json = r#"{"t":"sessions","names":[],"recovery":[
        {"stem":5,"label":"x","tasks":1,"age":0},
        {"label":"x","tasks":1,"age":0},
        {"stem":"s1","tasks":1,"age":0},
        {"stem":"s2","label":7,"tasks":1,"age":0},
        {"stem":"s3","label":"x","age":0},
        {"stem":"s4","label":"x","tasks":4294967296,"age":0},
        {"stem":"s5","label":"x","tasks":-1,"age":0},
        {"stem":"s6","label":"x","tasks":1,"age":-3},
        {"stem":"s7","label":"x","tasks":1},
        "flat",
        {"stem":"good","label":"autosaved","tasks":2,"age":7}
    ]}"#;
    assert_eq!(
        decode_event(KIND_CONTROL, json.as_bytes()),
        Some(Event::Sessions {
            names: Vec::new(),
            recovery: vec![RecoveryEntry {
                stem: "good".into(),
                label: "autosaved".into(),
                tasks: 2,
                age_secs: 7,
            }],
        })
    );
}

/// Watch frames require a Boolean `attached` field.
#[test]
fn watch_wire_form_requires_the_attached_flag() {
    let (k, p) = encode_command(&Command::Watch {
        id: Some(5),
        attached: true,
    });
    assert_eq!(k, KIND_CONTROL);
    assert_eq!(
        std::str::from_utf8(&p).unwrap(),
        r#"{"t":"watch","id":5,"attached":true}"#
    );
    let (_, p) = encode_command(&Command::Watch {
        id: None,
        attached: false,
    });
    assert_eq!(
        std::str::from_utf8(&p).unwrap(),
        r#"{"t":"watch","id":null,"attached":false}"#
    );
    for json in [
        r#"{"t":"watch","id":5}"#,                 // missing flag
        r#"{"t":"watch","id":null}"#,              // missing flag on unwatch
        r#"{"t":"watch","id":5,"attached":null}"#, // null is not a kind
        r#"{"t":"watch","id":5,"attached":1}"#,    // flag must be a boolean
    ] {
        assert_eq!(
            decode_command(KIND_CONTROL, json.as_bytes()),
            None,
            "should reject {json}"
        );
    }
}

/// `LoadRecovery` requires a string stem in its wire representation.
#[test]
fn load_recovery_wire_form() {
    let (k, p) = encode_command(&Command::LoadRecovery {
        stem: "20260714-093015-4242".into(),
    });
    assert_eq!(k, KIND_CONTROL);
    assert_eq!(
        std::str::from_utf8(&p).unwrap(),
        r#"{"t":"recover","stem":"20260714-093015-4242"}"#
    );
    for json in [
        r#"{"t":"recover"}"#,
        r#"{"t":"recover","stem":5}"#,
        r#"{"t":"recover","stem":null}"#,
    ] {
        assert_eq!(
            decode_command(KIND_CONTROL, json.as_bytes()),
            None,
            "should reject {json}"
        );
    }
}

/// A `Screen` event preserves formatted bytes, including non-UTF-8 values.
#[test]
fn screen_round_trips_raw_bytes() {
    let screen = Event::Screen(ScreenView {
        id: 9,
        lines: vec!["row0".into(), "row1".into()],
        formatted: vec![0x1b, b'[', b'm', 0, 255, b'x'],
        cursor: (3, 12),
        hide_cursor: false,
        wants_mouse: true,
        alt_screen: false,
        // The wire encodes this field independently of `alt_screen`.
        alt_scroll: true,
        scrollback: 42,
    });
    let (k, p) = encode_event(&screen);
    assert_eq!(k, KIND_SCREEN);
    assert_eq!(decode_event(k, &p), Some(screen));
}

/// Clipboard events preserve their source, target, and text.
#[test]
fn clipboard_copy_round_trips() {
    for ev in [
        Event::ClipboardCopy {
            id: 1,
            kind: ClipboardKind::Clipboard,
            text: "hello".into(),
        },
        Event::ClipboardCopy {
            id: 1 << 40,
            kind: ClipboardKind::Selection,
            text: "sélection λ 🦀".into(),
        },
        Event::ClipboardCopy {
            id: 2,
            kind: ClipboardKind::Primary,
            text: "primary".into(),
        },
        Event::ClipboardCopy {
            id: 7,
            kind: ClipboardKind::Clipboard,
            // Exercise control characters and paste-marker-shaped text.
            text: "line1\nline2\tcol\u{0}\u{1b}[201~end".into(),
        },
    ] {
        let (k, p) = encode_event(&ev);
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(decode_event(k, &p), Some(ev));
    }
}

/// Clipboard events encode every target with its OSC 52 selector.
#[test]
fn clipboard_copy_wire_form() {
    for (kind, wire) in [
        (
            ClipboardKind::Clipboard,
            r#"{"t":"clip","id":5,"k":"c","text":"aGk="}"#,
        ),
        (
            ClipboardKind::Primary,
            r#"{"t":"clip","id":5,"k":"p","text":"aGk="}"#,
        ),
        (
            ClipboardKind::Selection,
            r#"{"t":"clip","id":5,"k":"s","text":"aGk="}"#,
        ),
    ] {
        let (k, p) = encode_event(&Event::ClipboardCopy {
            id: 5,
            kind,
            text: "hi".into(),
        });
        assert_eq!(k, KIND_CONTROL);
        assert_eq!(std::str::from_utf8(&p).unwrap(), wire);
    }
}

/// Malformed clipboard event frames are rejected.
#[test]
fn malformed_clipboard_copy_is_rejected() {
    for json in [
        r#"{"t":"clip","id":1,"text":"aGk="}"#,  // missing kind
        r#"{"t":"clip","id":1,"k":"c"}"#,        // missing text
        r#"{"t":"clip","k":"c","text":"aGk="}"#, // missing id
        r#"{"t":"clip","id":"1","k":"c","text":"aGk="}"#, // id must be a number
        r#"{"t":"clip","id":1,"k":"x","text":"aGk="}"#, // unknown kind string
        r#"{"t":"clip","id":1,"k":"c","text":"!!!"}"#, // invalid base64
        r#"{"t":"clip","id":1,"k":"c","text":"/w=="}"#, // 0xFF: not UTF-8
        r#"{"t":"clip","id":1,"k":"c","text":["aGk="]}"#, // text must be a string
    ] {
        assert_eq!(
            decode_event(KIND_CONTROL, json.as_bytes()),
            None,
            "should reject {json}"
        );
    }
}
