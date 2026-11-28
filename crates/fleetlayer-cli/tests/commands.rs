//! Shared commands, end to end: a real control plane and two nodes as background processes, the laptop sharing
//! commands and the agents' node running them with `fleetlayer run`. Proves a run's arguments reach the program as
//! they are (a file by its path), its stdout, stderr and exit code come back, an argument off its type is refused by
//! the sharing node, the timeout stops it, the access policy refuses and asks, and an approved run goes ahead.
//!
//! The other programs come from the same build: `cargo build --workspace --bins` before the tests (CI does).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn bin_dir() -> PathBuf {
    let me = PathBuf::from(env!("CARGO_BIN_EXE_fleetlayer"));
    let dir = me.parent().unwrap().to_path_buf();
    for b in ["fleetlayer-control", "fleetlayer-node"] {
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
        std::thread::sleep(Duration::from_millis(200));
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
        Machine { home, instance: format!("cmd-{name}") }
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

    fn node(&self, name: &str, port: u16, token: &str, extra: &str) {
        let h = self.home.display();
        self.write(".config/fleetlayer/node-token", token);
        self.write(
            ".config/fleetlayer/node.yaml",
            &format!(
                "name: {name}\nskills: false\nplugins: []\ncontrol:\n  url: \"ws://127.0.0.1:{port}/v1/link\"\n  token_file: {h}/.config/fleetlayer/node-token\n{extra}"
            ),
        );
    }
}

struct Fleet {
    _root: tempfile::TempDir,
    laptop: Machine,
    agents: Machine,
}

impl Drop for Fleet {
    fn drop(&mut self) {
        for c in ["node", "control"] {
            self.laptop.service("stop", c);
            self.agents.service("stop", c);
        }
    }
}

const COMMANDS: &str = r#"commands:
  - name: echo
    description: Says a word and a file back
    run: [/bin/sh, -c, 'printf "%s|%s\n" "$1" "$(cat "$2")"; echo to-stderr >&2; exit 7', sh, "{word}", "{note}"]
    params:
      word: { type: choice, values: [hello, bye] }
      note: { type: file, extension: .txt, max_bytes: 100 }
    who: ["*@agents"]
  - name: slow
    run: [/bin/sleep, "30"]
    who: ["*@agents"]
    timeout: 2s
  - name: private
    run: [/bin/true]
    who: ["*@elsewhere"]
"#;

fn text(o: &Output) -> (String, String) {
    (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

#[test]
fn a_shared_command_runs_checked_and_decided() {
    let root = tempfile::Builder::new().prefix("flcmd").tempdir().unwrap();
    let f =
        Fleet { laptop: Machine::new(root.path(), "laptop"), agents: Machine::new(root.path(), "agents"), _root: root };
    let port = free_port();
    f.laptop.control(port);
    let enrol = |name: &str| {
        let out = f.laptop.control_cli(&["node", "add", name]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let (lt, at) = (enrol("laptop"), enrol("agents"));
    f.laptop.node("laptop", port, &lt, COMMANDS);
    f.agents.node("agents", port, &at, "");
    let apply = |policy: &str| {
        f.laptop.write("policy.yaml", policy);
        let out = f.laptop.control_cli(&["acl", "apply", f.laptop.home.join("policy.yaml").to_str().unwrap()]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    apply("version: 2\ndefault: allow\nask: []\n");
    f.laptop.service("start", "control");
    f.laptop.service("start", "node");
    f.agents.service("start", "node");
    wait_until("both nodes to link", 30, || f.laptop.linked() && f.agents.linked());

    // The list: what each takes, and nothing the laptop's own who: leaves out.
    let mut listed = String::new();
    wait_until("the laptop's commands to be listed", 20, || {
        listed = text(&f.agents.fleetlayer(&["commands", "laptop"])).0;
        listed.contains("echo@laptop")
    });
    assert!(listed.contains("--word: one of hello, bye"), "{listed}");
    assert!(listed.contains("slow@laptop") && !listed.contains("private@laptop"), "{listed}");
    assert!(!listed.contains("/bin/sh"), "the program isn't shared: {listed}");

    // A run: the file by its path, stdout and stderr apart, the program's exit code.
    std::fs::write(f.agents.home.join("note.txt"), "from the agents").unwrap();
    let out = f.agents.fleetlayer(&["run", "echo@laptop", "--word", "hello", "--note", "note.txt"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(7), "{stdout}\n{stderr}");
    assert_eq!(stdout, "hello|from the agents\n");
    assert!(stderr.contains("to-stderr"), "{stderr}");
    // The run's folder is gone afterwards.
    let state = f.laptop.home.join(".local/state/fleetlayer/commands");
    assert!(std::fs::read_dir(&state).map(|d| d.count()).unwrap_or(0) == 0, "a run's folder was left behind");

    // Off its type: refused by the laptop, nothing run.
    let out = f.agents.fleetlayer(&["run", "echo@laptop", "--word", "$(reboot)", "--note", "note.txt"]);
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(125), "{stderr}");
    assert!(stderr.contains("one of hello, bye"), "{stderr}");
    std::fs::write(f.agents.home.join("note.sh"), "x").unwrap();
    let out = f.agents.fleetlayer(&["run", "echo@laptop", "--word", "bye", "--note", "note.sh"]);
    assert_eq!(out.status.code(), Some(125), "{}", text(&out).1);

    // Its own who: list leaves this caller out.
    let out = f.agents.fleetlayer(&["run", "private@laptop"]);
    assert_eq!(out.status.code(), Some(125));

    // The timeout stops it.
    let started = Instant::now();
    let out = f.agents.fleetlayer(&["run", "slow@laptop"]);
    assert_eq!(out.status.code(), Some(124), "{}", text(&out).1);
    assert!(started.elapsed() < Duration::from_secs(20));

    // The policy refuses: not listed, not run.
    apply("version: 2\ndefault: allow\nask: []\ndeny:\n  - { who: \"*\", can: run, to: \"command:echo@*\" }\n");
    assert!(!text(&f.agents.fleetlayer(&["commands"])).0.contains("echo@laptop"));
    let out = f.agents.fleetlayer(&["run", "echo@laptop", "--word", "hello", "--note", "note.txt"]);
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(125), "{stderr}");
    // Refused before it's sent (it isn't offered to this caller), or by the control plane if the listing was stale.
    assert!(
        stderr.contains("no command echo@laptop") || stderr.contains("may not run command:echo@laptop"),
        "{stderr}"
    );

    // The policy asks: the run waits, the person sees the program and arguments, approves, and it goes ahead.
    apply("version: 2\ndefault: allow\nask:\n  - { who: \"*\", can: run, to: \"command:*\", for: once }\n");
    let child = f
        .agents
        .command("fleetlayer")
        .args(["run", "echo@laptop", "--word", "bye", "--note", "note.txt"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut id = String::new();
    let mut listing = String::new();
    wait_until("the run's approval request", 30, || {
        listing = text(&f.laptop.control_cli(&["approvals", "list"])).0;
        match listing.split_whitespace().next() {
            Some(first) if listing.contains("command:echo@laptop") => {
                id = first.to_string();
                true
            }
            _ => false,
        }
    });
    assert!(listing.contains("/bin/sh -c"), "{listing}");
    // Each parameter with its kind, and the file whole, for the person to read before approving.
    assert!(listing.contains("note (file): 'note.txt', 15 bytes, sha256"), "{listing}");
    assert!(listing.contains("word (choice): 'bye'"), "{listing}");
    assert!(listing.contains("| from the agents"), "the file is shown whole: {listing}");
    let out = f.laptop.control_cli(&["approvals", "approve", &id]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = child.wait_with_output().unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(7), "{stdout}\n{stderr}");
    assert_eq!(stdout, "bye|from the agents\n");
    assert!(stderr.contains("waiting for your person to approve request"), "{stderr}");
}
