use super::*;
use chrome::fixture::{Row, make};

fn home() -> PathBuf {
    PathBuf::from("/home/alice")
}

fn receive_x(dir: &Path) -> Receive {
    load(
        &home(),
        &json!({"receive": [{"set": "x", "write": [
            {"format": "env", "path": dir.join("x.env"), "values": {"AUTH_TOKEN": "auth_token", "CT0": "ct0"}},
            {"format": "netscape", "path": dir.join("cookies.txt")},
            {"format": "json", "path": dir.join("state.json")},
        ]}]}),
    )
    .unwrap()
    .receive
    .remove(0)
}

#[test]
fn config_is_checked() {
    let ok = load(
        &home(),
        &json!({"share": [{"set": "x", "domains": ["x.com"], "to": ["server"],
        "source": "file", "cookies_file": "~/c/Cookies"}]}),
    )
    .unwrap();
    assert_eq!(ok.share[0].cookies_file.as_deref(), Some(Path::new("/home/alice/c/Cookies")));
    assert_eq!(ok.share[0].profile, "Default");
    for good in ["bbc.co.uk", "someone.github.io", "api.x.com", ".x.com"] {
        assert!(domain_ok(good), "{good}");
    }
    let bad = [
        json!({"share": [{"set": "x", "domains": ["com"], "to": ["s"]}]}),
        json!({"share": [{"set": "x", "domains": ["*.x.com"], "to": ["s"]}]}),
        json!({"share": [{"set": "x", "domains": [], "to": ["s"]}]}),
        json!({"share": [{"set": "x", "domains": ["co.uk"], "to": ["s"]}]}),
        json!({"share": [{"set": "x", "domains": ["github.io"], "to": ["s"]}]}),
        json!({"share": [{"set": "x", "domains": ["x.com"], "to": []}]}),
        json!({"share": [{"set": "X Y", "domains": ["x.com"], "to": ["s"]}]}),
        json!({"share": [{"set": "x", "domains": ["x.com"], "to": ["s"], "sites": ["oops"]}]}),
        json!({"receive": [{"set": "x", "write": [{"format": "env", "path": "/tmp/a"}]}]}),
        json!({"receive": [{"set": "x", "write": [{"format": "env", "path": "/tmp/a", "values": {"1X": "a"}}]}]}),
        json!({"receive": [{"set": "x", "write": [{"format": "json", "path": "/tmp/a", "values": {"X": "a"}}]}]}),
        json!({"receive": [{"set": "x", "write": [{"format": "json", "path": "relative"}]}]}),
        json!({"receive": [{"set": "x", "write": [{"format": "json", "path": "/a"}, {"format": "netscape", "path": "/a"}]}]}),
    ];
    for b in bad {
        assert!(load(&home(), &b).is_err(), "{b}");
    }
}

/// A browser's database read for a share, carried in a bundle, and written on the target in all three formats.
#[tokio::test]
async fn from_the_browser_database_to_the_targets_files() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("Cookies");
    let far = Timestamp::now().as_second() + 86_400 * 300;
    make(
        &db,
        24,
        &chrome::derive(b"peanuts", 1),
        b"v10",
        &[
            Row { host: ".x.com", name: "auth_token", value: "abc123", expires_unix: Some(far), http_only: true },
            Row { host: ".x.com", name: "ct0", value: "def456", expires_unix: Some(far), http_only: false },
            Row { host: ".x.com", name: "guest_id", value: "g", expires_unix: Some(far), http_only: false },
            Row {
                host: ".example.com",
                name: "auth_token",
                value: "not this one",
                expires_unix: None,
                http_only: true,
            },
        ],
    );
    let share = load(
        &home(),
        &json!({"share": [{"set": "x", "domains": ["x.com"], "names": ["auth_token", "ct0"],
        "to": ["server"], "cookies_file": db}]}),
    )
    .unwrap()
    .share
    .remove(0);
    let plugin = Plugin {
        node: "laptop".into(),
        wanted: Mutex::new(HashMap::new()),
        home: d.path().to_path_buf(),
        receive: vec![],
        started: Timestamp::now(),
        received: Mutex::new(HashMap::new()),
        share_problems: Mutex::new(BTreeMap::new()),
        // As the Linux source would have them, so the test doesn't depend on this machine's keyring.
        keys: Mutex::new([("Chrome".to_string(), Ok(Keys::linux(None)))].into()),
        refused: Mutex::new(HashMap::new()),
    };
    let cookies = read_share(&plugin, &share, d.path()).await.unwrap();
    assert_eq!(cookies.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["auth_token", "ct0"]);
    // The bundle's item round-trips.
    let body = Body {
        set: "x".into(),
        domains: share.domains.clone(),
        read_at: Timestamp::now(),
        cookies,
        refresh_days: None,
    };
    let body: Body = serde_json::from_str(&serde_json::to_string(&body).unwrap()).unwrap();

    let out = d.path().join("out");
    let r = receive_x(&out);
    let (written, errors) = apply(&r, &body, &[]);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(written.len(), 3);
    let env = std::fs::read_to_string(out.join("x.env")).unwrap();
    assert!(env.contains("AUTH_TOKEN=abc123\nCT0=def456\n"), "{env}");
    let txt = std::fs::read_to_string(out.join("cookies.txt")).unwrap();
    assert!(txt.contains(&format!(".x.com\tTRUE\t/\tTRUE\t{far}\tauth_token\tabc123")), "{txt}");
    let state: Value = serde_json::from_str(&std::fs::read_to_string(out.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["cookies"][1]["value"], "def456");
}

#[test]
fn a_file_a_person_made_is_never_overwritten() {
    let d = tempfile::tempdir().unwrap();
    let r = receive_x(d.path());
    std::fs::write(d.path().join("x.env"), "MINE=1\n").unwrap();
    let c = |name: &str| Cookie {
        domain: ".x.com".into(),
        name: name.into(),
        value: "v".into(),
        path: "/".into(),
        expires: None,
        secure: true,
        http_only: false,
        same_site: None,
    };
    let body = Body {
        set: "x".into(),
        domains: vec!["x.com".into()],
        read_at: Timestamp::now(),
        cookies: vec![c("auth_token"), c("ct0")],
        refresh_days: None,
    };
    let (written, errors) = apply(&r, &body, &[]);
    assert_eq!(std::fs::read_to_string(d.path().join("x.env")).unwrap(), "MINE=1\n");
    assert_eq!(written.len(), 2);
    assert!(errors[0].contains("wasn't written by fleetlayer-cookies"), "{errors:?}");
    // Once it's recorded as ours (or moved away), it's written.
    let (_, errors) = apply(&r, &body, &written.iter().cloned().chain([d.path().join("x.env")]).collect::<Vec<_>>());
    assert!(errors.is_empty());
    assert!(std::fs::read_to_string(d.path().join("x.env")).unwrap().contains("CT0=v"));
}

#[test]
fn problems_a_target_reports() {
    let d = tempfile::tempdir().unwrap();
    let r = receive_x(d.path());
    let start = Timestamp::from_second(1_800_000_000).unwrap();
    let hours = |h: i64| Timestamp::from_second(1_800_000_000 + h * 3600).unwrap();
    let seen = |name: &str, expires: Option<i64>| Seen { domain: ".x.com".into(), name: name.into(), expires };

    // Waiting for the first bundle: fine for stale_hours, then a problem.
    let empty = Received::default();
    assert_eq!(receive_problem(&r, &empty, hours(47), start), None);
    assert!(
        receive_problem(&r, &empty, hours(49), start)
            .unwrap()
            .contains("nothing has arrived since this plugin started")
    );

    let mut st = Received {
        applied_at: Some(start),
        cookies: vec![seen("auth_token", Some(hours(100).as_second())), seen("ct0", None)],
        ..Default::default()
    };
    assert_eq!(receive_problem(&r, &st, hours(1), start), None);
    assert!(receive_problem(&r, &st, hours(49), start).unwrap().contains("is the source machine awake"));
    st.applied_at = Some(hours(99));
    assert!(receive_problem(&r, &st, hours(101), start).unwrap().contains("the auth_token cookie expired"));
    st.cookies.pop();
    assert!(receive_problem(&r, &st, hours(99), start).unwrap().contains("has no ct0 cookie"));
    // A browser source reads only every refresh_days unasked, so its sets go stale later.
    let kept = st.cookies.clone();
    st.cookies = vec![seen("auth_token", None), seen("ct0", None)];
    st.refresh_days = Some(14);
    st.applied_at = Some(start);
    assert_eq!(receive_problem(&r, &st, hours(15 * 24), start), None);
    assert!(receive_problem(&r, &st, hours(16 * 24 + 1), start).unwrap().contains("is the source machine awake"));
    st.refresh_days = None;
    st.cookies = kept;
    st.applied_at = Some(hours(99));
    st.error = Some("writing /x: denied".into());
    assert_eq!(receive_problem(&r, &st, hours(99), start).unwrap(), "cookies x: writing /x: denied");
}

#[test]
fn the_state_file_never_holds_values() {
    let st = Received {
        cookies: vec![Seen { domain: ".x.com".into(), name: "ct0".into(), expires: Some(1) }],
        ..Default::default()
    };
    let text = serde_json::to_string(&st).unwrap();
    assert!(!text.contains("value"), "{text}");
}

#[test]
fn the_browser_source_reads_only_named_cookies_from_chrome() {
    let ok = load(
        &home(),
        &json!({"share": [{"set": "x", "source": "browser", "domains": ["x.com"], "names": ["auth_token"], "to": ["s"]}]}),
    )
    .unwrap();
    assert_eq!(ok.share[0].source(), Source::Browser);
    assert_eq!(ok.share[0].refresh_days, 14);
    assert!(ok.share[0].playwright.cli.as_deref().unwrap().starts_with("/home/alice/.local/share/browser-mcp"));
    for bad in [
        json!({"share": [{"set": "x", "source": "browser", "domains": ["x.com"], "to": ["s"]}]}),
        json!({"share": [{"set": "x", "source": "browser", "browser": "brave", "domains": ["x.com"], "names": ["a"], "to": ["s"]}]}),
    ] {
        assert!(load(&home(), &bad).is_err(), "{bad}");
    }
    // The default follows the system: the browser itself on macOS, the cookie file elsewhere.
    let default =
        load(&home(), &json!({"share": [{"set": "x", "domains": ["x.com"], "names": ["a"], "to": ["s"]}]})).unwrap();
    assert_eq!(default.share[0].source(), if cfg!(target_os = "macos") { Source::Browser } else { Source::File });
}

#[test]
fn a_browser_read_is_due_on_first_delivery_a_request_or_the_refresh_only() {
    let t = |s: i64| Timestamp::from_second(1_800_000_000 + s).unwrap();
    let day = 86_400;
    assert_eq!(due(&Delivered::default(), None, 14, t(0)).as_deref(), Some("its first delivery"));
    let d = Delivered { delivered_at: Some(t(0)), answered: None };
    assert_eq!(due(&d, None, 14, t(13 * day)), None);
    assert_eq!(due(&d, None, 14, t(14 * day)).as_deref(), Some("the 14-day refresh"));
    let asked = (t(100), "bird whoami says signed out".to_string());
    assert_eq!(due(&d, Some(&asked), 14, t(200)).as_deref(), Some("it asked: bird whoami says signed out"));
    let answered = Delivered { delivered_at: Some(t(150)), answered: Some(t(100)) };
    assert_eq!(due(&answered, Some(&asked), 14, t(200)), None, "a request is answered once");
}

#[test]
fn reasons_reach_the_person_as_one_plain_line() {
    assert_eq!(plain_line("  please\nrefresh\t\u{1b}[31m now "), "please refresh [31m now");
    assert_eq!(plain_line(&"a".repeat(500)).len(), 300);
}
