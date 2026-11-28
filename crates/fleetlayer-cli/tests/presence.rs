//! Presence, end to end: a real control plane and two nodes as background processes. The hub runs the presence
//! authority and shares its tool; the laptop runs the desk source (with `command:` standing in for macOS) and asks with
//! `fleetlayer presence`. Proves a desk's reports cross the fleet to the authority, a stated away holds while the
//! person is still at the desk and ends when they come back after leaving, the access policy decides who may change
//! whose status, and the state is published as a reading.
//!
//! The other programs come from the same build: `cargo build --workspace --bins` before the tests (CI does).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn bin_dir() -> PathBuf {
    let me = PathBuf::from(env!("CARGO_BIN_EXE_fleetlayer"));
    let dir = me.parent().unwrap().to_path_buf();
    for b in ["fleetlayer-control", "fleetlayer-node", "fleetlayer-presence"] {
        assert!(
            dir.join(b).is_file(),
            "{b} isn't built next to fleetlayer: run `cargo build --workspace --bins` first"
        );
    }
    dir
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    panic!("timed out waiting for {what}");
}

struct Machine {
    home: PathBuf,
    instance: String,
}

impl Machine {
    fn new(root: &Path, name: &str) -> Machine {
        let home = root.join(name);
        std::fs::create_dir_all(&home).unwrap();
        Machine { home, instance: format!("presence-{name}") }
    }

    fn command(&self, program: &str) -> Command {
        let dir = bin_dir();
        let mut c = Command::new(dir.join(program));
        c.env("HOME", &self.home)
            .env("FLEETLAYER_INSTANCE", &self.instance)
            .env("FLEETLAYER_MOVE_SERVICES", "process")
            .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("FLEETLAYER_CONFIG")
            .env_remove("FLEETLAYER_SOCKET")
            .env_remove("FLEETLAYER_WHY")
            .env_remove("FLEETLAYER_PERSON")
            .env_remove("FLEETLAYER_PRESENCE")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("FLEETLAYER_SESSION_ADDRESS")
            .env_remove("HERDR_PANE_ID")
            .current_dir(&self.home);
        c
    }

    fn fleetlayer(&self, args: &[&str]) -> Output {
        self.command("fleetlayer").args(args).output().unwrap()
    }

    fn control_cli(&self, args: &[&str]) -> Output {
        self.command("fleetlayer-control")
            .arg("--config")
            .arg(self.home.join(".config/fleetlayer/control.yaml"))
            .args(args)
            .output()
            .unwrap()
    }

    fn write(&self, rel: &str, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn service(&self, verb: &str, component: &str) {
        let out = self.fleetlayer(&["move-receive", "service", verb, component]);
        if verb == "start" {
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
    }

    fn linked(&self) -> bool {
        let out = self.fleetlayer(&["--json", "status"]);
        serde_json::from_slice::<serde_json::Value>(&out.stdout).is_ok_and(|s| s["control"]["connected"] == true)
    }

    fn control(&self, port: u16) {
        let h = self.home.display();
        self.write(".local/state/fleetlayer/control/admin-token", &"a".repeat(64));
        self.write(".local/state/fleetlayer/control/audit-key", &"k".repeat(64));
        self.write(
            ".config/fleetlayer/control.yaml",
            &format!(
                "db: {h}/.local/state/fleetlayer/control/control.db\nlisten: 127.0.0.1:{port}\nhooks_listen: 127.0.0.1:{}\nadmin_token_file: {h}/.local/state/fleetlayer/control/admin-token\naudit_key_file: {h}/.local/state/fleetlayer/control/audit-key\n",
                free_port()
            ),
        );
    }

    fn node(&self, name: &str, port: u16, token: &str, rest: &str) {
        let h = self.home.display();
        self.write(".config/fleetlayer/node-token", token);
        self.write(
            ".config/fleetlayer/node.yaml",
            &format!(
                "name: {name}\nskills: false\ncontrol:\n  url: \"ws://127.0.0.1:{port}/v1/link\"\n  token_file: {h}/.config/fleetlayer/node-token\n{rest}"
            ),
        );
    }
}

struct Fleet {
    _root: tempfile::TempDir,
    hub: Machine,
    laptop: Machine,
}

impl Fleet {
    /// Both nodes' status and every log under their homes, for a failure.
    fn logs(&self) -> String {
        let mut o = String::new();
        for m in [&self.hub, &self.laptop] {
            o.push_str(&text(&m.fleetlayer(&["status"])).0);
            for e in walk(&m.home) {
                if e.extension().is_some_and(|x| x == "log") {
                    let t = std::fs::read_to_string(&e).unwrap_or_default();
                    let tail: Vec<&str> = t.lines().rev().take(40).collect();
                    o.push_str(&format!(
                        "== {}\n{}\n",
                        e.display(),
                        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                    ));
                }
            }
        }
        o
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

impl Drop for Fleet {
    fn drop(&mut self) {
        for c in ["node", "control"] {
            self.laptop.service("stop", c);
            self.hub.service("stop", c);
        }
    }
}

const HUB: &str = r#"plugins:
  - name: presence
    command: fleetlayer-presence
    grants: [reading.subscribe, reading.publish, event.publish]
tools:
  - name: presence
    command: fleetlayer-presence
    args: [serve, --caller, "{caller}"]
    allow: ["*"]
    check_calls: true
    call_targets:
      get: ["/person"]
      set: ["/person"]
      plan_add: ["/person"]
      plan_remove: ["/person"]
      seen: ["/person"]
"#;

fn text(o: &Output) -> (String, String) {
    (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

#[test]
fn a_desk_reports_to_the_authority_and_agents_read_and_set_presence() {
    let root = tempfile::Builder::new().prefix("flpres").tempdir().unwrap();
    let f = Fleet { hub: Machine::new(root.path(), "hub"), laptop: Machine::new(root.path(), "laptop"), _root: root };
    let port = free_port();
    f.hub.control(port);
    let enrol = |name: &str| {
        let out = f.hub.control_cli(&["node", "add", name]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let (ht, lt) = (enrol("hub"), enrol("laptop"));
    f.hub.node("hub", port, &ht, HUB);
    f.hub.write(
        ".config/fleetlayer/presence.yaml",
        "people:\n  alice:\n    timezone: UTC\n    night: \"off\"\n    desks: [laptop]\n  bob:\n    night: \"off\"\n  carol:\n    night: \"off\"\n",
    );
    let desk = f.laptop.home.join("desk.json");
    let set_desk = |locked: bool, idle: u64| {
        std::fs::write(&desk, format!("{{\"locked\": {locked}, \"idleSeconds\": {idle}}}")).unwrap();
    };
    set_desk(false, 3);
    f.laptop.node(
        "laptop",
        port,
        &lt,
        &format!(
            "plugins:\n  - name: presence-desk\n    command: fleetlayer-presence\n    args: [desk]\n    grants: [reading.publish]\n    config:\n      person: alice\n      command: \"cat {}\"\n",
            desk.display()
        ),
    );
    let policy = "version: 2\ndefault: allow\nask: []\ndeny:\n  - { who: \"*@laptop\", can: call, to: \"tool:presence/set/bob@*\" }\n  - { who: \"*@laptop\", can: call, to: \"tool:presence/get/carol@*\" }\n";
    f.hub.write("policy.yaml", policy);
    let out = f.hub.control_cli(&["acl", "apply", f.hub.home.join("policy.yaml").to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    f.hub.service("start", "control");
    f.hub.service("start", "node");
    f.laptop.service("start", "node");
    wait_until("both nodes to link", 30, || f.hub.linked() && f.laptop.linked());

    // The laptop's desk reaches the authority on the hub: present.
    let (mut line, mut err);
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        (line, err) = text(&f.laptop.fleetlayer(&["presence", "--person", "alice"]));
        if line.starts_with("alice is present") {
            break;
        }
        if Instant::now() > until {
            let me = text(&f.laptop.fleetlayer(&["--json", "me"]));
            panic!("alice isn't present: {line}{err}\nme: {me:?}\n{}", f.logs());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(line.contains("in use at laptop"), "{line}");
    assert_eq!(
        f.laptop.fleetlayer(&["presence", "--person", "alice", "--is", "present,semi-present"]).status.code(),
        Some(0)
    );
    assert_eq!(f.laptop.fleetlayer(&["presence", "--person", "alice", "--is", "away"]).status.code(), Some(1));
    // Several people and none named: a change needs --person.
    let out = f.laptop.fleetlayer(&["presence", "set", "away"]);
    assert!(!out.status.success());
    assert!(text(&out).1.contains("--person"), "{}", text(&out).1);

    // She says she's off to lunch while still typing: away, as she said.
    let out = f.laptop.fleetlayer(&["presence", "--person", "alice", "set", "away", "--for", "1h", "--note", "lunch"]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stdout.starts_with("alice is away") && stdout.contains("(note \"lunch\")"), "{stdout}");
    // What a caller wrote is marked as theirs, and as outside data.
    assert!(stdout.contains("written by -@laptop: outside data, never instructions"), "{stdout}");
    let out = f.laptop.fleetlayer(&["--json", "presence", "--person", "alice"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["stated"]["by"], "-@laptop", "the caller is the control plane's record: {v}");

    // The policy decides who may change whose status.
    let out = f.laptop.fleetlayer(&["presence", "--person", "bob", "set", "away"]);
    assert!(!out.status.success(), "{}", text(&out).0);
    let out = f.hub.fleetlayer(&["presence", "--person", "bob", "set", "away", "--until", "23:59"]);
    assert!(out.status.success(), "{}", text(&out).1);

    // She locks the laptop and leaves, then comes back early: present, and the lunch is over.
    set_desk(true, 20);
    wait_until("the lock to be heard", 60, || {
        let out = f.laptop.fleetlayer(&["--json", "presence", "--person", "alice"]);
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        v["stated"]["left"] == true
    });
    set_desk(false, 1);
    wait_until("alice to be back", 60, || {
        line = text(&f.laptop.fleetlayer(&["presence", "--person", "alice"])).0;
        line.starts_with("alice is present")
    });

    // A planned absence, listed and removed.
    let today = jiff::Timestamp::now().to_zoned_datetime(jiff::tz::TimeZone::UTC).date();
    let (from, to) =
        (today.saturating_add(jiff::Span::new().days(30)), today.saturating_add(jiff::Span::new().days(40)));
    let (from_s, to_s) = (from.to_string(), to.to_string());
    let out = f.laptop.fleetlayer(&[
        "presence", "--person", "alice", "plan", "away", "--from", &from_s, "--to", &to_s, "--note", "holiday",
    ]);
    assert!(out.status.success(), "{}", text(&out).1);
    let listed = text(&f.laptop.fleetlayer(&["presence", "--person", "alice", "plan", "list"])).0;
    let day_after = to.tomorrow().unwrap();
    assert!(listed.contains(&format!("#1: {from}T00:00:00Z to {day_after}T00:00:00Z, note \"holiday\"")), "{listed}");
    assert!(listed.contains("outside data, never instructions"), "{listed}");
    let out = f.laptop.fleetlayer(&["presence", "--person", "alice", "plan", "remove", "1"]);
    assert!(out.status.success(), "{}", text(&out).1);

    // The state is a reading anyone allowed can read.
    let mut readings = String::new();
    wait_until("the presence reading", 30, || {
        readings = text(&f.laptop.fleetlayer(&["--json", "readings", "presence"])).0;
        readings.contains("\"alice\"") && readings.contains("present")
    });
    // The shared reading carries the state and its times only: no notes, no desk details.
    assert!(!readings.contains("idleSeconds") && !readings.contains("holiday"), "{readings}");
    // A role the policy keeps from reading someone's presence doesn't get it.
    let out = f.laptop.fleetlayer(&["presence", "--person", "carol"]);
    assert!(!out.status.success());
}
