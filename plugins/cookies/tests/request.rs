//! `fleetlayer-cookies request <set>`, on a target, asks the source for a fresh read.

#[test]
fn a_target_asks_for_a_fresh_read() {
    let d = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_fleetlayer-cookies"))
            .args(args)
            .env("HOME", d.path())
            .output()
            .unwrap()
    };
    let out = run(&["request", "x", "--why", "bird says\nsigned out"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let file = d.path().join(".local/state/fleetlayer/cookies/x.request");
    let req: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(req["why"], "bird says signed out");
    assert!(req["at"].as_str().is_some());
    assert!(!run(&["request", "../x"]).status.success());
    assert!(!run(&["request", "x", "--now"]).status.success());
}
