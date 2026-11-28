//! `fleetlayer move`, end to end, between two home folders on this machine (`local:<folder>`, a second instance):
//! the real control plane, node, gateway and plugins run as background processes (`FLEETLAYER_MOVE_SERVICES=process`),
//! and each test proves what must survive a move does: the node's name and key (so the control plane accepts it and
//! secrets sealed to it still open), the control plane's database and tokens, the sign-in service's signing key, the
//! hub's and pages' files, and that whatever pointed at the old address now reaches the new one. Another test proves a
//! move that can't finish puts everything back.
//!
//! The other programs come from the same build: `cargo build --workspace --bins` before the tests (CI does).

#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

fn bin_dir() -> PathBuf {
    let me = PathBuf::from(env!("CARGO_BIN_EXE_fleetlayer"));
    let dir = me.parent().unwrap().to_path_buf();
    for b in [
        "fleetlayer-control",
        "fleetlayer-node",
        "fleetlayer-gateway",
        "fleetlayer-pages",
        "fleetlayer-signin",
        "fleetlayer-claude-auth-manager",
    ] {
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

fn http_get(port: u16, host: &str, path: &str) -> Option<(u16, String)> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut out = vec![];
    s.read_to_end(&mut out).ok()?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    Some((head.split(' ').nth(1)?.parse().ok()?, body.to_string()))
}

/// One "machine": a home folder and an instance name, with every program from this build on PATH.
struct Machine {
    home: PathBuf,
    instance: String,
}

impl Machine {
    /// `root/<name>`; the destination of `local:<root>/<name>` is the instance `move-<name>`.
    fn new(root: &Path, name: &str, instance: &str) -> Machine {
        let home = root.join(name);
        std::fs::create_dir_all(&home).unwrap();
        Machine { home, instance: instance.into() }
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
            .env_remove("FLEETLAYER_SOCKET");
        c
    }

    fn fleetlayer(&self, args: &[&str]) -> Output {
        self.command("fleetlayer").args(args).output().unwrap()
    }

    fn fleetlayer_with(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut c = self.command("fleetlayer");
        for (k, v) in env {
            c.env(k, v);
        }
        c.args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.fleetlayer(args);
        assert!(
            out.status.success(),
            "fleetlayer {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    fn write(&self, rel: &str, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("{}: {e}", self.path(rel).display()))
    }

    fn start(&self, component: &str) {
        self.ok(&["move-receive", "service", "start", component]);
    }

    fn stop(&self, component: &str) {
        let _ = self.fleetlayer(&["move-receive", "service", "stop", component]);
    }

    fn status(&self) -> Option<Value> {
        let out = self.fleetlayer(&["--json", "status"]);
        out.status.success().then(|| serde_json::from_slice(&out.stdout).ok()).flatten()
    }

    fn linked(&self) -> bool {
        self.status().is_some_and(|s| s["control"]["connected"] == true)
    }

    /// A control plane here on `port` with its settings file, the way `fleetlayer up` writes it.
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

    /// Enrol `name` in this machine's control plane; its token.
    fn enrol(&self, name: &str) -> String {
        let out = self
            .command("fleetlayer-control")
            .arg("--config")
            .arg(self.path(".config/fleetlayer/control.yaml"))
            .args(["node", "add", name])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// node.yaml here, linked to `control` with `token`, with these plugin items.
    fn node(&self, name: &str, control: Option<(u16, &str)>, plugins: &str) {
        let h = self.home.display();
        let mut y = format!("name: {name}\nskills: false\n");
        if let Some((port, token)) = control {
            self.write(".config/fleetlayer/node-token", token);
            y.push_str(&format!(
                "control:\n  url: \"ws://127.0.0.1:{port}/v1/link\"\n  token_file: {h}/.config/fleetlayer/node-token\n"
            ));
        }
        if plugins.is_empty() {
            y.push_str("plugins: []\n");
        } else {
            y.push_str("plugins:\n");
            y.push_str(plugins);
        }
        self.write(".config/fleetlayer/node.yaml", &y);
    }

    fn stop_all(&self) {
        for c in ["node", "gateway", "control"] {
            self.stop(c);
        }
    }
}

struct Fleet {
    _root: tempfile::TempDir,
    a: Machine,
    b: Machine,
}

impl Fleet {
    /// `a` is where things start; `b` is `local:<root>/b`.
    fn new() -> Fleet {
        let root = tempfile::Builder::new().prefix("flmv").tempdir().unwrap();
        let a = Machine::new(root.path(), "a", "src");
        let b = Machine::new(root.path(), "b", "move-b");
        Fleet { _root: root, a, b }
    }

    fn dest(&self) -> String {
        format!("local:{}", self.b.home.display())
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        self.a.stop_all();
        self.b.stop_all();
    }
}

fn hub_plugin(h: &str) -> String {
    format!(
        "  - name: claude-auth-manager\n    command: fleetlayer-claude-auth-manager\n    grants: [sync.publish, reading.publish]\n    config:\n      role: hub\n      claude: /bin/false\n      targets: []\n      hub:\n        listen: 127.0.0.1:0\n        login_file: {h}/.local/state/fleetlayer/claude-hub-login\n        trusted_proxies: [127.0.0.1]\n"
    )
}

#[test]
fn the_control_plane_moves_and_its_nodes_follow() {
    let f = Fleet::new();
    let (old, new) = (free_port(), free_port());
    f.a.control(old);
    let token = f.a.enrol("laptop");
    f.a.node("laptop", Some((old, &token)), "");
    f.a.start("control");
    f.a.start("node");
    wait_until("the node to link", 30, || f.a.linked());
    let key = f.a.status().unwrap()["publicKey"].as_str().unwrap().to_string();
    let admin = f.a.read(".local/state/fleetlayer/control/admin-token");

    // The dry run changes nothing.
    let plan = f.a.ok(&["move", "control", &f.dest(), "--port", &new.to_string(), "--dry-run"]);
    assert!(plan.contains("Dry run: nothing was changed."), "{plan}");
    assert!(plan.contains(&format!("ws://127.0.0.1:{new}/v1/link")), "{plan}");
    assert!(f.a.path(".config/fleetlayer/control.yaml").exists());
    assert!(!f.b.path(".config/fleetlayer").exists());

    let out = f.a.ok(&["move", "control", &f.dest(), "--port", &new.to_string()]);
    assert!(out.contains("It's healthy there."), "{out}");

    // Its files and settings are there, readdressed and rehomed; none are left here.
    let settings = f.b.read(".config/fleetlayer/control.yaml");
    assert!(settings.contains(&format!("listen: \"127.0.0.1:{new}\"")), "{settings}");
    assert!(
        settings.contains(&format!("{}/.local/state/fleetlayer/control/control.db", f.b.home.display())),
        "{settings}"
    );
    assert_eq!(f.b.read(".local/state/fleetlayer/control/admin-token"), admin);
    assert!(!f.b.path(".local/state/fleetlayer/control/moved-to").exists());
    assert!(!f.a.path(".config/fleetlayer/control.yaml").exists());
    assert!(!f.a.path(".local/state/fleetlayer/control").exists());

    // The node followed: it links to the new address with the same token and key (the control plane refuses a
    // changed key), and kept the address in its node.yaml.
    wait_until("the node to link to the moved control plane", 30, || f.a.linked());
    assert_eq!(f.a.status().unwrap()["publicKey"], key.as_str());
    let node_yaml = f.a.read(".config/fleetlayer/node.yaml");
    assert!(node_yaml.contains(&format!("ws://127.0.0.1:{new}/v1/link")), "{node_yaml}");
    // The enrolment survived: the moved control plane still knows the node.
    let out = f.b.command("fleetlayer-control").args(["node", "list"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("laptop"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_node_moves_with_its_name_and_key() {
    let f = Fleet::new();
    let port = free_port();
    f.a.control(port);
    let token = f.a.enrol("laptop");
    let h = f.a.home.display().to_string();
    f.a.write(".local/state/fleetlayer/claude-hub-login", &"l".repeat(48));
    f.a.node("laptop", Some((port, &token)), &hub_plugin(&h));
    f.a.start("control");
    f.a.start("node");
    wait_until("the node to link", 30, || f.a.linked());
    let key = f.a.status().unwrap()["publicKey"].as_str().unwrap().to_string();

    // A secret sealed to the node before the move.
    let recipient: age::x25519::Recipient = key.parse().unwrap();
    let enc = age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient)).unwrap();
    let mut sealed = vec![];
    let mut w = enc.wrap_output(&mut sealed).unwrap();
    w.write_all(b"a year-long token").unwrap();
    w.finish().unwrap();

    // Another node may not be there already.
    f.b.write(".config/fleetlayer/node.yaml", "name: other\n");
    let out = f.a.fleetlayer(&["move", "node", &f.dest()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("there's a node there already"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(f.a.linked(), "a refused move changes nothing");
    std::fs::remove_file(f.b.path(".config/fleetlayer/node.yaml")).unwrap();

    let out = f.a.ok(&["move", "node", &f.dest()]);
    assert!(out.contains("It's healthy there."), "{out}");
    let s = f.b.status().expect("the node runs there");
    assert_eq!(s["node"], "laptop");
    assert_eq!(s["publicKey"], key.as_str());
    assert_eq!(s["control"]["connected"], true);
    assert!(f.a.status().is_none(), "and not here");
    assert!(!f.a.path(".config/fleetlayer/node-key").exists());
    // Its plugins' files came along, and node.yaml names them in the new home.
    assert_eq!(f.b.read(".local/state/fleetlayer/claude-hub-login"), "l".repeat(48));
    assert!(
        f.b.read(".config/fleetlayer/node.yaml")
            .contains(&format!("{}/.local/state/fleetlayer/claude-hub-login", f.b.home.display()))
    );

    // The secret still opens with the key that arrived.
    let text = f.b.read(".config/fleetlayer/node-key");
    let id: age::x25519::Identity = text.lines().find(|l| l.starts_with("AGE-SECRET-KEY-")).unwrap().parse().unwrap();
    let dec = age::Decryptor::new(sealed.as_slice()).unwrap();
    let mut plain = String::new();
    dec.decrypt(std::iter::once(&id as &dyn age::Identity)).unwrap().read_to_string(&mut plain).unwrap();
    assert_eq!(plain, "a year-long token");
}

#[test]
fn pages_moves_and_the_old_address_reaches_it() {
    let f = Fleet::new();
    let (h, gw, new) = (f.a.home.display().to_string(), free_port(), free_port());
    f.a.write(".local/state/fleetlayer/pages/login", &"p".repeat(48));
    f.a.write("Fleetlayer/personal/pages/.fleetlayer-space", "");
    f.a.write("Fleetlayer/personal/apps/.fleetlayer-space", "");
    f.a.write("Fleetlayer/personal/pages/2026-11-28-plan/index.md", "# The plan\n");
    f.a.node(
        "laptop",
        None,
        &format!("  # Pages.\n  - name: pages\n    command: fleetlayer-pages\n    config:\n      listen: 127.0.0.1:0\n      base_url: http://pages.localhost:{gw}\n      apps_domain: apps.localhost\n      login_file: {h}/.local/state/fleetlayer/pages/login\n      trusted_proxies: [127.0.0.1]\n      spaces:\n        personal: {{ dir: {h}/Fleetlayer/personal/pages, apps: {h}/Fleetlayer/personal/apps }}\n"),
    );
    f.a.write(".config/fleetlayer/gateway.yaml", &format!("listen: 127.0.0.1:{gw}\nservices:\n  {{}}\nalerts: {{}}\n"));
    f.a.start("node");
    f.a.start("gateway");
    wait_until("pages through the gateway", 30, || {
        http_get(gw, "pages.localhost", "/healthz").is_some_and(|(c, _)| c == 200)
    });

    // Without a node there, nothing happens.
    let out = f.a.fleetlayer(&["move", "pages", &f.dest(), "--port", &new.to_string()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("there's no node there"));
    assert!(f.a.read(".config/fleetlayer/node.yaml").contains("\n  - name: pages"));

    f.b.node("server", None, "  - name: other\n    command: /bin/true\n    enabled: false\n");
    f.b.start("node");
    wait_until("the node there", 20, || f.b.status().is_some());
    let out = f.a.ok(&["move", "apps", &f.dest(), "--port", &new.to_string()]);
    assert!(out.contains("It's healthy there."), "{out}");

    // Served there, on its new port, from its own files.
    assert_eq!(http_get(new, "localhost", "/healthz").map(|r| r.0), Some(200));
    assert_eq!(f.b.read(".local/state/fleetlayer/pages/login"), "p".repeat(48));
    assert_eq!(f.b.read("Fleetlayer/personal/pages/2026-11-28-plan/index.md"), "# The plan\n");
    let there = f.b.read(".config/fleetlayer/node.yaml");
    assert!(there.contains("- name: pages") && there.contains("- name: other"), "{there}");
    assert!(there.contains(&format!("listen: \"127.0.0.1:{new}\"")), "{there}");
    // Here: its block is kept as a comment, and the old address still works, through the gateway's route.
    let here = f.a.read(".config/fleetlayer/node.yaml");
    assert!(here.contains("# Moved to local:") && here.contains("  # - name: pages"), "{here}");
    assert!(
        f.a.read(".config/fleetlayer/gateway.yaml").contains(&format!("pages: {{ url: \"http://127.0.0.1:{new}\""))
    );
    wait_until("pages through the old address", 30, || {
        http_get(gw, "pages.localhost", "/healthz").is_some_and(|(c, _)| c == 200)
    });
    assert!(!f.a.path(".local/state/fleetlayer/pages/login").exists());
    // The pages themselves are the person's: copied, never deleted here.
    assert_eq!(f.a.read("Fleetlayer/personal/pages/2026-11-28-plan/index.md"), "# The plan\n");
}

#[test]
fn the_hub_moves_with_its_folder() {
    let f = Fleet::new();
    let (h, new) = (f.a.home.display().to_string(), free_port());
    f.a.write(".local/state/fleetlayer/claude-hub-login", &"l".repeat(48));
    f.a.node("laptop", None, &hub_plugin(&h));
    f.a.start("node");
    wait_until("the hub's folder", 30, || f.a.path(".local/state/fleetlayer/claude-hub").exists());
    f.a.write(".local/state/fleetlayer/claude-hub/kept-by-the-test", "accounts");
    f.b.node("server", None, "");
    f.b.start("node");
    wait_until("the node there", 20, || f.b.status().is_some());

    let out = f.a.ok(&["move", "hub", &f.dest(), "--port", &new.to_string(), "--keep-old"]);
    assert!(out.contains("It's healthy there."), "{out}");
    assert_eq!(http_get(new, "localhost", "/healthz").map(|r| r.0), Some(200));
    assert_eq!(f.b.read(".local/state/fleetlayer/claude-hub/kept-by-the-test"), "accounts");
    assert_eq!(f.b.read(".local/state/fleetlayer/claude-hub-login"), "l".repeat(48));
    // --keep-old: renamed here, not deleted.
    let kept: Vec<String> = std::fs::read_dir(f.a.path(".local/state/fleetlayer"))
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("claude-hub.moved-"))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
}

#[test]
fn sign_in_moves_with_its_signing_key() {
    let f = Fleet::new();
    let (h, control, new) = (f.a.home.display().to_string(), free_port(), free_port());
    f.a.control(control);
    f.a.start("control");
    f.a.write(".config/fleetlayer/signin-token", &"t".repeat(40));
    f.a.node(
        "laptop",
        None,
        &format!("  - name: signin\n    command: fleetlayer-signin\n    config:\n      dir: {h}/.local/state/fleetlayer/signin\n      issuer: http://signin.localhost:7680\n      control_plane: http://127.0.0.1:{control}\n      token_file: {h}/.config/fleetlayer/signin-token\n      listen: 127.0.0.1:0\n"),
    );
    f.a.start("node");
    wait_until("sign-in's key", 30, || f.a.path(".local/state/fleetlayer/signin/keys/signing.pem").exists());
    let key = f.a.read(".local/state/fleetlayer/signin/keys/signing.pem");
    f.b.node("server", None, "");
    f.b.start("node");
    wait_until("the node there", 20, || f.b.status().is_some());

    let out = f.a.ok(&["move", "sign-in", &f.dest(), "--port", &new.to_string()]);
    assert!(out.contains("It's healthy there."), "{out}");
    assert!(out.contains("Passkeys belong to its passkey domain"), "{out}");
    assert_eq!(f.b.read(".local/state/fleetlayer/signin/keys/signing.pem"), key, "tokens it signed still verify");
    assert_eq!(f.b.read(".config/fleetlayer/signin-token"), "t".repeat(40));
    let (code, jwks) = http_get(new, "signin.localhost:7680", "/jwks").unwrap();
    assert_eq!(code, 200, "{jwks}");
    assert!(!f.a.path(".local/state/fleetlayer/signin").exists());
}

#[test]
fn the_gateway_moves_and_the_node_checks_it_there() {
    let f = Fleet::new();
    let (old, new) = (free_port(), free_port());
    f.a.write(
        ".config/fleetlayer/gateway.yaml",
        &format!("listen: 127.0.0.1:{old}\nservices:\n  {{}}\nalerts: {{}}\n"),
    );
    f.a.node("laptop", None, "");
    let y = f.a.read(".config/fleetlayer/node.yaml");
    f.a.write(".config/fleetlayer/node.yaml", &format!("{y}gateway: \"http://127.0.0.1:{old}\"\n"));
    f.a.start("gateway");
    f.a.start("node");
    wait_until("the gateway", 20, || http_get(old, "fleetlayer.localhost", "/healthz").is_some());

    let out = f.a.ok(&["move", "gateway", &f.dest(), "--port", &new.to_string()]);
    assert!(out.contains("It's healthy there."), "{out}");
    assert_eq!(http_get(new, "fleetlayer.localhost", "/healthz").map(|r| r.0), Some(200));
    assert!(http_get(old, "fleetlayer.localhost", "/healthz").is_none(), "stopped here");
    assert!(f.a.read(".config/fleetlayer/node.yaml").contains(&format!("gateway: \"http://127.0.0.1:{new}\"")));
    assert!(f.b.read(".config/fleetlayer/gateway.yaml").contains(&format!("listen: \"127.0.0.1:{new}\"")));
    assert!(!f.a.path(".config/fleetlayer/gateway.yaml").exists());
}

#[test]
fn a_move_that_cant_finish_puts_everything_back() {
    let f = Fleet::new();
    let old = free_port();
    f.a.control(old);
    let token = f.a.enrol("laptop");
    f.a.node("laptop", Some((old, &token)), "");
    f.a.start("control");
    f.a.start("node");
    wait_until("the node to link", 30, || f.a.linked());
    let db = std::fs::read(f.a.path(".local/state/fleetlayer/control/control.db")).unwrap().len();

    // The port it would use there is taken, so it never gets healthy there.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let out = f.a.fleetlayer(&["move", "control", &f.dest(), "--port", &port, "--health-timeout", "5"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("It's running here again"), "{err}");

    // Nothing is left there; everything is here, running, and the node is linked to it again.
    assert!(!f.b.path(".config/fleetlayer/control.yaml").exists());
    assert!(!f.b.path(".local/state/fleetlayer/control/control.db").exists());
    assert!(f.a.path(".config/fleetlayer/control.yaml").exists());
    assert!(!f.a.path(".local/state/fleetlayer/control/moved-to").exists());
    assert!(std::fs::read(f.a.path(".local/state/fleetlayer/control/control.db")).unwrap().len() >= db);
    wait_until("the node to link here again", 60, || f.a.linked());
    assert!(f.a.read(".config/fleetlayer/node.yaml").contains(&format!("ws://127.0.0.1:{old}/v1/link")));
}

/// A control plane here, linked to by this machine's node: (old port, the node's token).
fn linked_control(f: &Fleet) -> u16 {
    let old = free_port();
    f.a.control(old);
    let token = f.a.enrol("laptop");
    f.a.node("laptop", Some((old, &token)), "");
    f.a.start("control");
    f.a.start("node");
    wait_until("the node to link", 30, || f.a.linked());
    old
}

#[test]
fn nodes_keep_the_old_address_until_the_move_is_confirmed() {
    let f = Fleet::new();
    let old = linked_control(&f);
    // Healthy there, then the move fails anyway (as a check after health would): the copy there is undone after
    // the node may already have linked to it. The node must come back to the old address, and node.yaml keep it.
    let new = free_port().to_string();
    let out = f
        .a
        .fleetlayer_with(&[("FLEETLAYER_MOVE_TEST_FAIL", "health")], &["move", "control", &f.dest(), "--port", &new]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("It's running here again"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!f.b.path(".local/state/fleetlayer/control/control.db").exists());
    let log = f.a.read(".local/state/fleetlayer/run/node.log");
    assert!(
        log.contains(&format!("linked to the control plane url=ws://127.0.0.1:{new}/v1/link")),
        "the node reached the new copy first:\n{log}"
    );
    wait_until("the node to link here again", 180, || f.a.linked());
    let y = f.a.read(".config/fleetlayer/node.yaml");
    assert!(y.contains(&format!("ws://127.0.0.1:{old}/v1/link")), "{y}");
    assert!(!f.a.path(".local/state/fleetlayer/control/move-proof").exists());
    assert!(!f.a.path(".local/state/fleetlayer/moves/control.json").exists(), "the journal is gone");
}

#[test]
fn a_destination_that_cant_be_confirmed_stopped_isnt_restarted_here() {
    let f = Fleet::new();
    linked_control(&f);
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let fail = [("FLEETLAYER_MOVE_TEST_FAIL", "undo")];
    let out = f.a.fleetlayer_with(&fail, &["move", "control", &f.dest(), "--port", &port, "--health-timeout", "3"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("isn't started here") && err.contains("--rollback --force"), "{err}");
    // Not running here: two control planes never run.
    let running = f.a.ok(&["move-receive", "service", "running", "control"]);
    assert!(running.contains("\"running\":false"), "{running}");
    // A new move waits for this one; --rollback still can't confirm; --force takes the person's word.
    let out = f.a.fleetlayer(&["move", "control", &f.dest(), "--port", &port]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("didn't finish"), "{}", String::from_utf8_lossy(&out.stderr));
    let out = f.a.fleetlayer_with(&fail, &["move", "control", &f.dest(), "--rollback"]);
    assert!(!out.status.success());
    f.a.ok(&["move", "control", &f.dest(), "--rollback", "--force"]);
    wait_until("the node to link here again", 60, || f.a.linked());
}

#[test]
fn an_interrupted_move_can_be_finished_or_undone() {
    let f = Fleet::new();
    let (h, new) = (f.a.home.display().to_string(), free_port());
    f.a.write(".local/state/fleetlayer/claude-hub-login", &"l".repeat(48));
    f.a.node("laptop", None, &hub_plugin(&h));
    f.a.start("node");
    f.b.node("server", None, "");
    f.b.start("node");
    wait_until("both nodes", 20, || f.a.status().is_some() && f.b.status().is_some());
    let stop = [("FLEETLAYER_MOVE_TEST_EXIT_AFTER", "copied")];
    let args = ["move", "hub", &f.dest(), "--port", &new.to_string()];

    // Interrupted after the copy, then undone.
    let out = f.a.fleetlayer_with(&stop, &args);
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(f.a.path(".local/state/fleetlayer/moves/plugin-claude-auth-manager.json").exists());
    let out = f.a.fleetlayer(&args);
    assert!(String::from_utf8_lossy(&out.stderr).contains("--resume"), "{}", String::from_utf8_lossy(&out.stderr));
    f.a.ok(&["move", "hub", &f.dest(), "--rollback"]);
    assert!(f.a.read(".config/fleetlayer/node.yaml").contains("\n  - name: claude-auth-manager"));
    assert!(!f.b.path(".local/state/fleetlayer/claude-hub-login").exists());
    assert!(!f.b.read(".config/fleetlayer/node.yaml").contains("claude-auth-manager"));
    wait_until("the hub here again", 30, || {
        f.a.status()
            .is_some_and(|s| s["plugins"].as_array().unwrap().iter().any(|p| p["name"] == "claude-auth-manager"))
    });

    // Interrupted again, then finished.
    let out = f.a.fleetlayer_with(&stop, &args);
    assert_eq!(out.status.code(), Some(3));
    let out = f.a.ok(&["move", "hub", &f.dest(), "--resume"]);
    assert!(out.contains("It's healthy there."), "{out}");
    assert_eq!(http_get(new, "localhost", "/healthz").map(|r| r.0), Some(200));
    assert!(!f.a.path(".local/state/fleetlayer/moves/plugin-claude-auth-manager.json").exists());
    assert!(!f.a.path(".local/state/fleetlayer/claude-hub-login").exists());
}

#[test]
fn plain_http_on_a_public_address_needs_saying_so() {
    let f = Fleet::new();
    let old = free_port();
    f.a.write(
        ".config/fleetlayer/gateway.yaml",
        &format!("listen: 127.0.0.1:{old}\nservices:\n  {{}}\nalerts: {{}}\n"),
    );
    let out = f.a.fleetlayer(&["move", "gateway", &f.dest(), "--listen", "203.0.113.9:7680", "--dry-run"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("a public address"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = f.a.ok(&["move", "gateway", &f.dest(), "--listen", "203.0.113.9:7680", "--public", "--dry-run"]);
    assert!(out.contains("Dry run"), "{out}");
}
