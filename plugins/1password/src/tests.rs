use super::*;

/// A stand-in for 1Password's `op`: vaults from `vaults.json`, items from `items-<vault id>.json`, values from
/// `values/<reference with / as _>`, every call appended to `calls`, and `fail` makes every call fail.
fn fake_op(dir: &Path) -> Op {
    std::fs::create_dir_all(dir.join("values")).unwrap();
    let script = dir.join("op");
    let d = dir.display();
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
echo "$*" >> {d}/calls
[ -n "$OP_SERVICE_ACCOUNT_TOKEN" ] || {{ echo '[ERROR] 2026/11/28 10:00:00 no token' >&2; exit 1; }}
[ -e {d}/fail ] && {{ echo '[ERROR] 2026/11/28 10:00:00 Too many requests' >&2; exit 1; }}
case "$1 $2" in
  "vault list") cat {d}/vaults.json ;;
  "item list") cat {d}/items-$4.json ;;
  "read --no-newline")
    [ -e {d}/readfail ] && {{ echo '[ERROR] 2026/11/28 10:00:00 dial tcp: lookup my.1password.com: host not found' >&2; exit 1; }}
    f={d}/values/$(printf %s "$3" | tr '/' '_')
    if [ -e "$f" ]; then cat "$f"; else echo "[ERROR] 2026/11/28 10:00:00 \"x\" isn't an item in the \"v\" vault" >&2; exit 1; fi ;;
  *) echo "unexpected $*" >&2; exit 2 ;;
esac
"#
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("vaults.json"), r#"[{"id":"v1","name":"github"}]"#).unwrap();
    Op { bin: script.display().to_string() }
}

fn items(dir: &Path, version: u64) {
    std::fs::write(
        dir.join("items-v1.json"),
        format!(r#"[{{"id":"i1","title":"bot","version":{version},"updated_at":"2026-11-28T00:00:00Z"}}]"#),
    )
    .unwrap();
}

fn value(dir: &Path, r: &str, v: &str) {
    std::fs::write(dir.join("values").join(r.replace('/', "_")), v).unwrap();
}

fn reads(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("calls")).unwrap_or_default().lines().filter(|l| l.starts_with("read")).count()
}

const TOKEN: &str = "op://github/bot/token";

#[tokio::test]
async fn items_are_read_once_and_again_only_when_their_version_moves() {
    let d = tempfile::tempdir().unwrap();
    let op = fake_op(d.path());
    items(d.path(), 1);
    value(d.path(), TOKEN, "ghp_first_value_0001");
    let refs: BTreeSet<String> = [TOKEN.to_string()].into();
    let tokens = vec![Some("sa-token".to_string())];
    let mut st = SourceState::default();
    let r = collect(&op, &tokens, &refs, &mut st).await;
    assert!(r.stuck.is_empty() && r.missing.is_empty(), "{:?} {:?}", r.problems, r.missing);
    assert_eq!(r.values[TOKEN].value, "ghp_first_value_0001");
    let r = collect(&op, &tokens, &refs, &mut st).await;
    assert_eq!(r.values[TOKEN].value, "ghp_first_value_0001");
    assert_eq!(reads(d.path()), 1, "an unchanged item isn't read again");
    items(d.path(), 2);
    value(d.path(), TOKEN, "ghp_second_value_002");
    let r = collect(&op, &tokens, &refs, &mut st).await;
    assert_eq!(r.values[TOKEN].value, "ghp_second_value_002");
    assert_eq!(reads(d.path()), 2);
}

#[tokio::test]
async fn a_missing_item_is_left_out_and_a_failure_keeps_the_last_value_or_holds_back() {
    let d = tempfile::tempdir().unwrap();
    let op = fake_op(d.path());
    items(d.path(), 1);
    value(d.path(), TOKEN, "ghp_first_value_0001");
    let refs: BTreeSet<String> = [TOKEN.to_string(), "op://github/gone/token".into(), "op://nowhere/x/y".into()].into();
    let tokens = vec![Some("sa-token".to_string())];
    let mut st = SourceState::default();
    let r = collect(&op, &tokens, &refs, &mut st).await;
    assert!(r.stuck.is_empty(), "{:?}", r.problems);
    assert_eq!(r.values.len(), 1);
    let missing: Vec<&str> = r.missing.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(missing, ["op://github/gone/token", "op://nowhere/x/y"]);
    // op fails (a rate limit): what was read before is still delivered, nothing is called missing.
    std::fs::write(d.path().join("fail"), "").unwrap();
    st.vaults_at = None;
    let r = collect(&op, &tokens, &refs, &mut st).await;
    assert!(r.problems.iter().any(|p| p.contains("Too many requests")), "{:?}", r.problems);
    assert_eq!(r.values[TOKEN].value, "ghp_first_value_0001");
    assert!(!r.stuck.contains(TOKEN) && !r.missing.iter().any(|(m, _)| m == TOKEN));
    assert!(r.stuck.contains("op://nowhere/x/y"), "a vault no token could list just now isn't called missing");
    // With nothing read before, it's held back rather than left out.
    let r = collect(&op, &tokens, &refs, &mut SourceState::default()).await;
    assert!(r.stuck.contains(TOKEN) && r.values.is_empty());
}

#[tokio::test]
async fn an_ambiguous_item_holds_back_only_itself() {
    let d = tempfile::tempdir().unwrap();
    let op = fake_op(d.path());
    std::fs::write(
        d.path().join("items-v1.json"),
        r#"[{"id":"i1","title":"bot","version":1},{"id":"i2","title":"twin","version":1},{"id":"i3","title":"twin","version":1}]"#,
    )
    .unwrap();
    value(d.path(), TOKEN, "ghp_first_value_0001");
    let refs: BTreeSet<String> = [TOKEN.to_string(), "op://github/twin/token".into()].into();
    let r = collect(&op, &[Some("t".into())], &refs, &mut SourceState::default()).await;
    assert_eq!(r.values[TOKEN].value, "ghp_first_value_0001", "the other item still goes");
    assert!(r.stuck.contains("op://github/twin/token"));
    assert!(r.problems.iter().any(|p| p.contains("2 items called")), "{:?}", r.problems);
}

#[tokio::test]
async fn a_recreated_item_at_the_same_version_is_read_again() {
    let d = tempfile::tempdir().unwrap();
    let op = fake_op(d.path());
    items(d.path(), 1);
    value(d.path(), TOKEN, "ghp_first_value_0001");
    let refs: BTreeSet<String> = [TOKEN.to_string()].into();
    let tokens = vec![Some("t".to_string())];
    let mut st = SourceState::default();
    collect(&op, &tokens, &refs, &mut st).await;
    std::fs::write(d.path().join("items-v1.json"), r#"[{"id":"i9","title":"bot","version":1}]"#).unwrap();
    value(d.path(), TOKEN, "ghp_recreated_value_9");
    let r = collect(&op, &tokens, &refs, &mut st).await;
    assert_eq!(r.values[TOKEN].value, "ghp_recreated_value_9");
}

#[tokio::test]
async fn an_error_that_merely_says_not_found_never_removes_an_item() {
    let d = tempfile::tempdir().unwrap();
    let op = fake_op(d.path());
    items(d.path(), 1);
    std::fs::write(d.path().join("readfail"), "").unwrap();
    let refs: BTreeSet<String> = [TOKEN.to_string()].into();
    let r = collect(&op, &[Some("t".into())], &refs, &mut SourceState::default()).await;
    assert!(r.missing.is_empty(), "{:?}", r.missing);
    assert!(r.stuck.contains(TOKEN));
}

#[test]
fn versions_only_go_up_even_when_the_clock_steps_back() {
    let mut disk = SourceDisk::default();
    let t1 = Timestamp::from_second(1_800_000_000).unwrap();
    let v1 = disk.next_version(t1);
    let v2 = disk.next_version(Timestamp::from_second(1_700_000_000).unwrap());
    assert!(v2 > v1);
    let back: SourceDisk = serde_json::from_str(&serde_json::to_string(&disk).unwrap()).unwrap();
    let mut back = back;
    assert!(back.next_version(t1) > v2, "the counter survives a restart");
}

#[test]
fn a_token_file_others_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("token");
    std::fs::write(&f, "ops_x\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(op::read_token(&f).unwrap_err().contains("chmod 600"));
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(op::read_token(&f).unwrap(), "ops_x");
}

#[tokio::test]
async fn the_token_reaches_op_in_its_environment_not_its_arguments() {
    let d = tempfile::tempdir().unwrap();
    let op = fake_op(d.path());
    items(d.path(), 1);
    value(d.path(), TOKEN, "ghp_first_value_0001");
    let refs: BTreeSet<String> = [TOKEN.to_string()].into();
    collect(&op, &[Some("ops_secret_token".to_string())], &refs, &mut SourceState::default()).await;
    let calls = std::fs::read_to_string(d.path().join("calls")).unwrap();
    assert!(!calls.contains("ops_secret_token"), "{calls}");
    // With no token at all, op refuses: the fake checks the variable.
    let r = collect(&op, &[Some(String::new())], &refs, &mut SourceState::default()).await;
    assert!(r.stuck.contains(TOKEN), "{:?}", r.problems);
}

fn delivered(r: &str, v: &str) -> Delivered {
    Delivered { reference: r.into(), value: v.into(), updated_at: None }
}

#[test]
fn files_are_written_removed_when_a_grant_goes_and_a_persons_file_is_left_alone() {
    let d = tempfile::tempdir().unwrap();
    let env = d.path().join("github.env");
    let file = d.path().join("token");
    let outputs = vec![
        Output {
            format: Format::Env,
            path: env.clone(),
            values: [("GITHUB_TOKEN".to_string(), TOKEN.to_string())].into(),
            reference: None,
        },
        Output { format: Format::File, path: file.clone(), values: BTreeMap::new(), reference: Some(TOKEN.into()) },
    ];
    let (written, missing, errors) = apply_outputs(&outputs, &[delivered(TOKEN, "it's x")], &[]);
    assert!(errors.is_empty() && missing.is_empty(), "{errors:?}");
    assert_eq!(written, vec![env.clone(), file.clone()]);
    assert!(std::fs::read_to_string(&env).unwrap().contains(r"GITHUB_TOKEN='it'\''s x'"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "it's x");
    // The grant is taken away: both files go.
    let (written, missing, errors) = apply_outputs(&outputs, &[], &written);
    assert!(errors.is_empty());
    assert_eq!(missing, vec![TOKEN.to_string()]);
    assert!(written.is_empty() && !env.exists() && !file.exists());
    // A person's file at that path is never overwritten.
    std::fs::write(&file, "mine").unwrap();
    let (written, _, errors) = apply_outputs(&outputs, &[delivered(TOKEN, "v")], &[]);
    assert!(errors[0].contains("wasn't written by fleetlayer-1password"), "{errors:?}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine");
    // A file the config no longer names is removed.
    assert_eq!(written, vec![env.clone()]);
    let (_, _, errors) = apply_outputs(&[], &[], &written);
    assert!(errors.is_empty() && !env.exists());
}

#[test]
fn an_env_file_of_ours_is_recognised_without_the_state_file() {
    let d = tempfile::tempdir().unwrap();
    let env = d.path().join("github.env");
    let outputs = vec![Output {
        format: Format::Env,
        path: env.clone(),
        values: [("GITHUB_TOKEN".to_string(), TOKEN.to_string())].into(),
        reference: None,
    }];
    apply_outputs(&outputs, &[delivered(TOKEN, "old")], &[]);
    // The state file is gone (written list empty): the file is still rewritten, and still removed with its grant.
    let (written, _, errors) = apply_outputs(&outputs, &[delivered(TOKEN, "new")], &[]);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(std::fs::read_to_string(&env).unwrap().contains("GITHUB_TOKEN=new"));
    assert_eq!(written, vec![env.clone()]);
    apply_outputs(&outputs, &[], &[]);
    assert!(!env.exists());
}

#[test]
fn a_multi_line_value_goes_in_a_file_not_an_env_file() {
    let got: BTreeMap<&str, &str> = [(TOKEN, "line1\nline2")].into();
    let e = env_file(&[("K".to_string(), TOKEN.to_string())].into(), &got).unwrap_err();
    assert!(e.contains("write it as a file"), "{e}");
}

#[test]
fn config_is_checked() {
    let home = Path::new("/home/a");
    let ok = json!({
        "source": {"token_files": ["~/.config/fleetlayer/1password/token"],
            "roles": [{"role": "everyday", "nodes": ["agents", "mac-agents"], "read": [TOKEN]}]},
        "receive": {"from": "secrets", "write": [{"format": "env", "path": "~/s/github.env", "values": {"GITHUB_TOKEN": TOKEN}}]}
    });
    let c = load(home, &ok).unwrap();
    assert_eq!(c.source.unwrap().token_files[0], PathBuf::from("/home/a/.config/fleetlayer/1password/token"));
    assert_eq!(c.receive.unwrap().write[0].path, PathBuf::from("/home/a/s/github.env"));
    for (bad, says) in [
        (json!({"source": {"token_files": [], "roles": []}}), "token_files"),
        (json!({"source": {"token_files": ["/t"], "roles": [{"role": "a", "nodes": [], "read": []}]}}), "nodes"),
        (
            json!({"source": {"token_files": ["/t"], "roles": [{"role": "a", "nodes": ["n"], "read": ["x"]}]}}),
            "reference",
        ),
        (json!({"receive": {"from": "s", "write": [{"format": "env", "path": "/e", "values": {}}]}}), "needs values"),
        (json!({"receive": {"from": "s", "write": [{"format": "file", "path": "/f"}]}}), "one ref"),
        (json!({"receive": {"from": "s", "write": [{"format": "file", "path": "rel", "ref": TOKEN}]}}), "absolute"),
        (json!({"receive": {}}), "from"),
    ] {
        let e = load(home, &bad).unwrap_err();
        assert!(e.contains(says), "{bad}: {e}");
    }
}

#[test]
fn roles_on_several_nodes_and_nodes_in_several_roles_merge_per_node() {
    let roles = vec![
        Role { role: "everyday".into(), nodes: vec!["agents".into(), "mac".into()], read: vec![TOKEN.into()] },
        Role { role: "infra".into(), nodes: vec!["agents".into()], read: vec!["op://cloud/dns/token".into()] },
    ];
    let by = grants_by_node(&roles);
    assert_eq!(by["agents"].len(), 2);
    assert_eq!(by["mac"].iter().collect::<Vec<_>>(), [TOKEN]);
}

#[test]
fn a_target_says_when_nothing_has_arrived_or_a_file_lost_its_grant() {
    let rc = Receive { stale_hours: 48, ..Default::default() };
    let start = Timestamp::from_second(1_800_000_000).unwrap();
    let later = Timestamp::from_second(1_800_000_000 + 49 * 3600).unwrap();
    let st = Received::default();
    assert!(receive_problem(&rc, &st, start, start).is_none());
    assert!(receive_problem(&rc, &st, later, start).unwrap().contains("nothing has arrived"));
    let st = Received { applied_at: Some(later), missing: vec![TOKEN.into()], ..Default::default() };
    assert!(receive_problem(&rc, &st, later, start).unwrap().contains("isn't granted"));
}

#[tokio::test]
async fn applying_registers_the_values_and_forgetting_removes_everything() {
    let d = tempfile::tempdir().unwrap();
    let env = d.path().join("out/github.env");
    let plugin = Plugin {
        home: d.path().to_path_buf(),
        node: "agents".into(),
        receive: Some(Receive {
            from: "secrets".into(),
            write: vec![Output {
                format: Format::Env,
                path: env.clone(),
                values: [("GITHUB_TOKEN".to_string(), TOKEN.to_string())].into(),
                reference: None,
            }],
            stale_hours: 48,
        }),
        started: Timestamp::now(),
        received: Mutex::new(Received::default()),
        source_problem: Mutex::new(None),
        refused: Mutex::new(None),
    };
    let body =
        Body { node: "agents".into(), read_at: Timestamp::now(), secrets: vec![delivered(TOKEN, "ghp_value_123456")] };
    assert_eq!(plugin.apply(7, body).await.unwrap(), 1);
    let (reg, _) = registry::load(d.path());
    assert_eq!(reg, vec![Secret { reference: TOKEN.into(), value: "ghp_value_123456".into() }]);
    assert!(env.exists());
    let saved = std::fs::read_to_string(d.path().join(".local/state/fleetlayer/1password/received.json")).unwrap();
    assert!(!saved.contains("ghp_value"), "the state file holds no values: {saved}");
    plugin.forget().await.unwrap();
    assert!(!env.exists());
    assert!(registry::load(d.path()).0.is_empty());
}
