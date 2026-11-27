use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::engine::{Answer, Asker, Engine};
use crate::hosts::{Cidr, Pattern, Target, is_private, parse_authority};
use crate::settings::{self, Mode};
use crate::sni::{self, Sni};

fn t(host: &str, port: u16) -> Target {
    Target { host: host.into(), port }
}

#[test]
fn patterns_match_names_ports_and_subdomains() {
    let p = Pattern::parse("*.github.com").unwrap();
    assert!(p.matches(&t("api.github.com", 443)));
    assert!(p.matches(&t("a.b.github.com", 80)));
    assert!(!p.matches(&t("github.com", 443)), "*. is only what's under it");
    assert!(!p.matches(&t("evilgithub.com", 443)));
    assert!(!p.matches(&t("api.github.com", 22)), "no port means 443 and 80");
    let p = Pattern::parse("github.com:22").unwrap();
    assert!(p.matches(&t("github.com", 22)) && !p.matches(&t("github.com", 443)));
    let p = Pattern::parse("Example.COM.").unwrap();
    assert_eq!(p.to_string(), "example.com");
    assert!(Pattern::parse("[2001:db8::1]:8443").unwrap().matches(&t("2001:db8::1", 8443)));
    assert!(Pattern::parse("2001:db8::1").unwrap().matches(&t("2001:db8::1", 443)));
    assert!(Pattern::parse("intranet").is_err(), "a bare name could be a local one");
    assert!(Pattern::parse("a.*.com").is_err());
    assert!(Pattern::parse("*.1.2.3.4").is_err());
    assert_eq!(parse_authority("API.Example.com:8443", 443), Some(t("api.example.com", 8443)));
    assert_eq!(parse_authority("[::1]:80", 443), Some(t("::1", 80)));
    assert_eq!(parse_authority("example.com:0", 443), None);
    assert_eq!(parse_authority("exa mple.com", 443), None);
    assert_eq!(t("example.com", 443).policy_name(), "host:example.com");
    assert_eq!(t("example.com", 22).policy_name(), "host:example.com:22");
}

#[test]
fn private_addresses_are_the_lan_the_tailnet_and_metadata() {
    for ip in [
        "10.1.2.3",
        "192.168.1.1",
        "172.16.0.1",
        "100.100.100.100",
        "169.254.169.254",
        "127.0.0.1",
        "::1",
        "fd7a:115c:a1e0::1",
        "fe80::1",
        "::ffff:10.0.0.1",
        "0.0.0.0",
    ] {
        assert!(is_private(ip.parse().unwrap()), "{ip}");
    }
    for ip in ["1.1.1.1", "140.82.112.3", "2606:4700::1111", "100.128.0.1"] {
        assert!(!is_private(ip.parse().unwrap()), "{ip}");
    }
    let c = Cidr::parse("10.5.0.0/16").unwrap();
    assert!(c.contains("10.5.7.37".parse().unwrap()) && !c.contains("10.6.0.1".parse().unwrap()));
    assert!(c.contains("::ffff:10.5.0.1".parse().unwrap()));
    assert!(Cidr::parse("10.0.0.0/33").is_err());
}

#[test]
fn the_server_name_comes_from_the_client_hello() {
    let hello = sni::client_hello(Some("api.github.com"));
    assert_eq!(sni::parse(&hello), Sni::Name("api.github.com".into()));
    assert_eq!(sni::parse(&hello[..10]), Sni::NeedMore);
    assert_eq!(sni::parse(&sni::client_hello(None)), Sni::NoName);
    assert_eq!(sni::parse(b"GET / HTTP/1.1\r\n"), Sni::NotTls);
    // The same handshake split across two records.
    let hs = &hello[5..];
    let (a, b) = hs.split_at(20);
    let mut split = vec![0x16, 0x03, 0x01];
    split.extend_from_slice(&(a.len() as u16).to_be_bytes());
    split.extend_from_slice(a);
    split.extend_from_slice(&[0x16, 0x03, 0x01]);
    split.extend_from_slice(&(b.len() as u16).to_be_bytes());
    split.extend_from_slice(b);
    assert_eq!(sni::parse(&split), Sni::Name("api.github.com".into()));
}

#[test]
fn the_owner_of_a_connection_comes_from_proc_net() {
    let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
               0: 0100007F:0C38 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 1 1\n   \
               1: 2507050A:D431 0101A8C0:01BB 01 00000000:00000000 00:00000000 00000000  1002        0 2 1\n";
    let o = crate::procnet::tcp_owner(&[tcp.to_string()], "10.5.7.37:54321".parse().unwrap()).unwrap();
    assert_eq!(o.uid, 1002);
    assert_eq!(o.remote, "192.168.1.1:443".parse().unwrap(), "the address it first dialled");
    let mapped: SocketAddr = "[::ffff:10.5.7.37]:54321".parse().unwrap();
    assert_eq!(crate::procnet::tcp_owner(&[tcp.to_string()], mapped).unwrap().uid, 1002);
    let tcp6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
                0: 00000000000000000000000001000000:9C40 00000000000000000000000001000000:0C38 01 00000000:00000000 00:00000000 00000000  1003        0 3 1\n";
    assert_eq!(crate::procnet::tcp_owner(&[tcp6.to_string()], "[::1]:40000".parse().unwrap()).unwrap().uid, 1003);
    let udp = "  sl  local_address rem_address   st\n  5: 00000000:A000 00000000:0000 07 00000000:00000000 00:00000000 00000000  1004 0 4\n";
    assert_eq!(crate::procnet::udp_owner(&[udp.to_string()], "127.0.0.1:40960".parse().unwrap()), Some(1004));
    let passwd = "root:x:0:0::/root:/bin/sh\ntriage:x:1002:1002::/home/triage:/bin/bash\n";
    assert_eq!(crate::procnet::Users::parse(passwd).get(&1002).map(String::as_str), Some("triage"));
}

#[test]
fn dns_questions_are_read_and_refused() {
    let q = crate::dns::query(0xabcd, "Pastebin.com");
    let (name, qtype, end) = crate::dns::question(&q).unwrap();
    assert_eq!((name.as_str(), qtype, end), ("pastebin.com", 1, q.len()));
    let r = crate::dns::refused(&q, end);
    assert_eq!(&r[..2], &[0xab, 0xcd]);
    assert_eq!(r[3] & 0x0f, 5, "REFUSED");
    assert_eq!(r[2] & 0x80, 0x80, "an answer");
    assert_eq!(&r[12..], &q[12..], "with its question");
    assert!(crate::dns::question(&[0u8; 5]).is_none());
}

const SETTINGS: &str = r#"
mode: enforce
roles:
  triage:
    users: [triage]
    node: agents-triage
    presets: [github]
    hosts: ["api.example.com", "*.docs.example.org", "db.example.net:5432"]
    networks: ["10.5.0.0/16"]
  worker:
    users: [worker, builder]
    mode: log
    presets: [anthropic]
"#;

#[test]
fn settings_are_checked_whole() {
    let s = settings::parse(SETTINGS).unwrap();
    assert_eq!(s.roles.len(), 2);
    assert_eq!(s.role_of_user("builder").unwrap().name, "worker");
    assert_eq!(s.role("worker").unwrap().mode, Mode::Log);
    assert_eq!(s.role("triage").unwrap().mode, Mode::Enforce);
    assert!(settings::parse("").unwrap().roles.is_empty());
    let e = settings::parse(
        "roles:\n  a: { users: [x], presets: [nope], hosts: ['bad name'] }\n  b: { users: [x, root] }\nports: { proxy: 3129 }\n",
    )
    .unwrap_err();
    for want in [
        "no preset is named \"nope\"",
        "bad name",
        "user x is in two roles",
        "\"root\" can't be",
        "four different ports",
    ] {
        assert!(e.contains(want), "{want} in {e}");
    }
    assert!(settings::parse("roles: { a: { users: [x], unknown: 1 } }").is_err());
}

struct FakeAsker {
    answers: StdMutex<Vec<Answer>>,
    asked: StdMutex<Vec<(String, String, String)>>,
}

#[async_trait]
impl Asker for FakeAsker {
    async fn ask(&self, node: &str, target: &str, why: &str) -> Result<Answer, String> {
        self.asked.lock().unwrap().push((node.into(), target.into(), why.into()));
        let mut a = self.answers.lock().unwrap();
        if a.is_empty() { Err("no control plane".into()) } else { Ok(a.remove(0)) }
    }
}

fn engine(text: &str) -> (Arc<Engine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Engine::new(settings::parse(text).unwrap(), dir.path().to_path_buf()), dir)
}

#[tokio::test]
async fn decisions_follow_the_role_its_list_and_its_mode() {
    let (e, _d) = engine(SETTINGS);
    let d = e.decide(Some("triage"), &t("api.github.com", 443)).await;
    assert!(d.go && d.word == "allowed" && d.reason.contains("preset github"), "{d:?}");
    let d = e.decide(Some("triage"), &t("pastebin.com", 443)).await;
    assert!(!d.go && d.word == "denied" && d.reason.contains("isn't on role triage's list"), "{d:?}");
    let d = e.decide(Some("triage"), &t("db.example.net", 5432)).await;
    assert!(d.go);
    assert!(!e.decide(Some("triage"), &t("db.example.net", 443)).await.go);
    // Log mode lets it through and says what enforcing would do.
    let d = e.decide(Some("builder"), &t("pastebin.com", 443)).await;
    assert!(d.go && d.word == "would_deny", "{d:?}");
    // Users outside every role aren't filtered; an unknown owner is refused.
    assert!(e.decide(Some("nathan"), &t("pastebin.com", 443)).await.go);
    assert!(!e.decide(None, &t("api.github.com", 443)).await.go);
    // Private addresses: only for exact entries, approvals or the role's networks.
    let wild = e.decide(Some("triage"), &t("a.docs.example.org", 443)).await;
    assert!(wild.address_ok("1.1.1.1".parse().unwrap()));
    assert!(!wild.address_ok("192.168.1.10".parse().unwrap()));
    assert!(wild.address_ok("10.5.7.50".parse().unwrap()), "in the role's networks");
    let trusted = e.decide(Some("nathan"), &t("x.example.com", 443)).await;
    assert!(trusted.address_ok(IpAddr::from([192, 168, 1, 1])));
    // DNS names follow the same list, whatever the port.
    assert!(e.decide_name(Some("triage"), "db.example.net").await.go);
    assert!(e.decide_name(Some("triage"), "x.docs.example.org").await.go);
    assert!(!e.decide_name(Some("triage"), "pastebin.com").await.go);
    assert!(e.decide_name(Some("worker"), "pastebin.com").await.go, "log mode");
}

#[tokio::test]
async fn an_agent_asks_and_once_approved_the_host_stays_allowed() {
    let (e, dir) = engine(SETTINGS);
    let asker = Arc::new(FakeAsker {
        answers: StdMutex::new(vec![Answer::Waiting(Some("ap_123".into())), Answer::Allowed]),
        asked: StdMutex::default(),
    });
    *e.asker.write().await = asker.clone();
    let why = "the vendor's API for the invoice import";
    let r = e.ask("triage", &t("api.vendor.io", 443), why).await;
    assert_eq!(r.status, "waiting");
    assert_eq!(r.request.as_deref(), Some("ap_123"));
    assert!(r.message.contains("fleetlayer-control approvals approve ap_123"), "{}", r.message);
    assert_eq!(asker.asked.lock().unwrap()[0], ("agents-triage".into(), "host:api.vendor.io".into(), why.into()));
    let d = e.decide(Some("triage"), &t("api.vendor.io", 443)).await;
    assert!(!d.go && d.reason.contains("waiting for a person to approve host:api.vendor.io (request ap_123)"), "{d:?}");
    // The recheck finds it approved (pretend a minute went by).
    e.state.lock().await.roles.get_mut("triage").unwrap().pending.get_mut("host:api.vendor.io").unwrap().checked_at -=
        jiff::SignedDuration::from_secs(120);
    e.recheck().await;
    let d = e.decide(Some("triage"), &t("api.vendor.io", 443)).await;
    assert!(d.go && d.reason == "approved", "{d:?}");
    assert!(e.decide_name(Some("triage"), "api.vendor.io").await.go);
    // Remembered across a restart, even with no control plane to ask.
    let again = Engine::new(settings::parse(SETTINGS).unwrap(), dir.path().to_path_buf());
    assert!(again.decide(Some("triage"), &t("api.vendor.io", 443)).await.go);
    // Already allowed: nothing to ask.
    assert_eq!(e.ask("triage", &t("api.github.com", 443), why).await.status, "already");
    // A role without a node can't ask; a refusal is passed on.
    assert_eq!(e.ask("worker", &t("x.example.com", 443), why).await.status, "unavailable");
    *e.asker.write().await = Arc::new(FakeAsker {
        answers: StdMutex::new(vec![Answer::Refused("deny[0]".into())]),
        asked: StdMutex::default(),
    });
    let r = e.ask("triage", &t("pastebin.com", 443), why).await;
    assert!(r.status == "refused" && r.message.contains("deny[0]"), "{r:?}");
    // At most ten new hosts an hour: a hijacked agent can't flood its person.
    *e.asker.write().await = Arc::new(FakeAsker {
        answers: StdMutex::new((0..20).map(|i| Answer::Waiting(Some(format!("ap_{i}")))).collect()),
        asked: StdMutex::default(),
    });
    let mut statuses = vec![];
    for i in 0..12 {
        statuses.push(e.ask("triage", &t(&format!("h{i}.example.io"), 443), why).await.status);
    }
    assert_eq!(statuses.iter().filter(|s| **s == "waiting").count(), 8, "two already used above: {statuses:?}");
    let r = e.ask("triage", &t("h99.example.io", 443), why).await;
    assert!(r.status == "refused" && r.message.contains("10 new hosts"), "{r:?}");
    // Asking again about one that's waiting isn't a new request.
    assert_eq!(e.ask("triage", &t("h0.example.io", 443), why).await.status, "waiting");
}

/// A server that echoes what it reads, on loopback.
async fn echo() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut b = [0u8; 4096];
                loop {
                    match s.read(&mut b).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let _ = s.write_all(&b[..n]).await;
                        }
                    }
                }
            });
        }
    });
    a
}

async fn serve(e: Arc<Engine>, kind: crate::proxy::Kind, user: &str, remote: SocketAddr) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    let user = user.to_string();
    let who: crate::proxy::Who = Arc::new(move |_, _| Some((user.clone(), remote)));
    let server = crate::proxy::Server { engine: e, who, own: Arc::new(vec![a]) };
    tokio::spawn(server.serve(l, kind));
    a
}

async fn read_all(s: &mut TcpStream) -> String {
    let mut out = vec![];
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).to_string()
}

#[tokio::test]
async fn the_proxy_tunnels_allowed_hosts_and_explains_refusals() {
    let up = echo().await;
    let text = format!(
        "mode: enforce\nroles:\n  triage:\n    users: [triage]\n    networks: [127.0.0.0/8]\n    hosts: [\"127.0.0.1:{}\"]\n",
        up.port()
    );
    let (e, dir) = engine(&text);
    let proxy = serve(e.clone(), crate::proxy::Kind::Proxy, "triage", up).await;

    let mut s = TcpStream::connect(proxy).await.unwrap();
    s.write_all(format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\nhello", up.port()).as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 64];
    let mut got = String::new();
    while !got.ends_with("hello") {
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0, "{got}");
        got.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(got.starts_with("HTTP/1.1 200"), "{got}");
    drop(s);

    let mut s = TcpStream::connect(proxy).await.unwrap();
    s.write_all(b"CONNECT pastebin.com:443 HTTP/1.1\r\n\r\n").await.unwrap();
    let got = read_all(&mut s).await;
    assert!(got.starts_with("HTTP/1.1 403") && got.contains("X-Fleetlayer-Egress: refused"), "{got}");
    assert!(got.contains("fleetlayer-egress ask pastebin.com --why"), "{got}");

    // The API: status, and why.
    let mut s = TcpStream::connect(proxy).await.unwrap();
    s.write_all(b"GET /status HTTP/1.1\r\nHost: egress.fleetlayer\r\n\r\n").await.unwrap();
    let got = read_all(&mut s).await;
    assert!(got.contains("\"role\":\"triage\"") && got.contains("\"mode\":\"enforce\""), "{got}");
    let mut s = TcpStream::connect(proxy).await.unwrap();
    s.write_all(b"GET http://egress.fleetlayer/why?host=pastebin.com HTTP/1.1\r\nHost: egress.fleetlayer\r\n\r\n")
        .await
        .unwrap();
    let got = read_all(&mut s).await;
    assert!(got.contains("\"decision\":\"denied\"") && got.contains("isn't on role triage"), "{got}");
    let mut s = TcpStream::connect(proxy).await.unwrap();
    let body = r#"{"host":"pastebin.com","why":"short"}"#;
    s.write_all(format!("POST /ask HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
        .await
        .unwrap();
    assert!(read_all(&mut s).await.contains("too short"));

    // Every connection is in the audit log, with its bytes.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let lines = crate::audit::read(&dir.path().join("audit"), 1, jiff::Timestamp::now());
    let ok = lines.iter().find(|l| l.decision == "allowed").expect("the allowed tunnel");
    assert_eq!((ok.user.as_str(), ok.via.as_str(), ok.up), ("triage", "connect", 5));
    assert!(lines.iter().any(|l| l.decision == "denied" && l.host == "pastebin.com"));
}

#[tokio::test]
async fn redirected_tls_is_decided_by_its_server_name_and_never_decrypted() {
    let up = echo().await;
    // The firewall redirected a connection that first asked for the echo server's port.
    let text = format!(
        "mode: enforce\nroles:\n  triage:\n    users: [triage]\n    networks: [127.0.0.0/8]\n    hosts: [\"localhost:{}\"]\n",
        up.port()
    );
    let (e, _dir) = engine(&text);
    let tls = serve(e.clone(), crate::proxy::Kind::Tls, "triage", up).await;

    let hello = sni::client_hello(Some("localhost"));
    let mut s = TcpStream::connect(tls).await.unwrap();
    s.write_all(&hello).await.unwrap();
    let mut back = vec![0u8; hello.len()];
    tokio::time::timeout(std::time::Duration::from_secs(5), s.read_exact(&mut back)).await.unwrap().unwrap();
    assert_eq!(back, hello, "the ClientHello is passed on untouched");

    let mut s = TcpStream::connect(tls).await.unwrap();
    s.write_all(&sni::client_hello(Some("pastebin.com"))).await.unwrap();
    let mut out = vec![];
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_end(&mut out)).await;
    assert_eq!(out, sni::ACCESS_DENIED_ALERT, "refused with a TLS alert a client prints");

    // No server name: decided by the address dialled, which isn't listed.
    let mut s = TcpStream::connect(tls).await.unwrap();
    s.write_all(&sni::client_hello(None)).await.unwrap();
    let mut out = vec![];
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_end(&mut out)).await;
    assert_eq!(out, sni::ACCESS_DENIED_ALERT);
}

#[tokio::test]
async fn redirected_http_is_decided_by_its_host_header() {
    let up = echo().await;
    let text = format!(
        "mode: enforce\nroles:\n  triage:\n    users: [triage]\n    networks: [127.0.0.0/8]\n    hosts: [\"localhost:{}\"]\n",
        up.port()
    );
    let (e, _dir) = engine(&text);
    let http = serve(e.clone(), crate::proxy::Kind::Http, "triage", up).await;
    let req = format!("GET / HTTP/1.1\r\nHost: localhost:{}\r\n\r\n", up.port());
    // It goes up as the connection's only request.
    let sent = format!("GET / HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n", up.port());
    let mut s = TcpStream::connect(http).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut back = vec![0u8; sent.len()];
    tokio::time::timeout(std::time::Duration::from_secs(5), s.read_exact(&mut back)).await.unwrap().unwrap();
    assert_eq!(String::from_utf8(back).unwrap(), sent);
    let mut s = TcpStream::connect(http).await.unwrap();
    s.write_all(b"GET / HTTP/1.1\r\nHost: pastebin.com\r\n\r\n").await.unwrap();
    assert!(read_all(&mut s).await.starts_with("HTTP/1.1 403"));
}

#[tokio::test]
async fn log_mode_lets_it_through_and_records_what_it_would_refuse() {
    let up = echo().await;
    let (e, dir) = engine("mode: log\nroles:\n  triage:\n    users: [triage]\n");
    let proxy = serve(e.clone(), crate::proxy::Kind::Proxy, "triage", up).await;
    let mut s = TcpStream::connect(proxy).await.unwrap();
    s.write_all(format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\nhi", up.port()).as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 128];
    let n = s.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    drop(s);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let lines = crate::audit::read(&dir.path().join("audit"), 1, jiff::Timestamp::now());
    assert!(lines.iter().any(|l| l.decision == "would_deny" && l.host == "127.0.0.1"), "{lines:?}");
}

#[test]
fn suggest_proposes_presets_wildcards_and_hosts_from_the_log() {
    let s = settings::parse("roles:\n  triage:\n    users: [triage]\n").unwrap();
    let line = |host: &str, port: Option<u16>, via: &str| crate::audit::Line {
        at: jiff::Timestamp::now(),
        user: "triage".into(),
        role: Some("triage".into()),
        via: via.into(),
        host: host.into(),
        port,
        decision: "would_deny".into(),
        reason: String::new(),
        up: 0,
        down: 0,
        ms: 0,
    };
    let lines = vec![
        line("api.github.com", Some(443), "tls"),
        line("a.cdn.example.com", Some(443), "tls"),
        line("b.cdn.example.com", Some(443), "tls"),
        line("c.example.com", Some(443), "connect"),
        line("db.example.net", Some(5432), "connect"),
        line("only-dns.example.org", None, "dns"),
        line("1.0.0.10.in-addr.arpa", None, "dns"),
    ];
    let out = crate::audit::suggest(&lines, &s, None);
    assert!(out.contains("presets: [github]"), "{out}");
    assert!(out.contains("\"*.example.com\""), "{out}");
    assert!(out.contains("\"db.example.net:5432\""), "{out}");
    assert!(out.contains("only-dns.example.org\"   # 0 connections, DNS only"), "{out}");
    assert!(!out.contains("arpa"), "{out}");
    let parsed: serde_json::Value = yaml_serde::from_str(&out).unwrap();
    assert!(parsed["roles"]["triage"]["hosts"].is_array(), "{out}");
}

#[test]
fn the_firewall_redirects_and_refuses_only_restricted_users() {
    let s = settings::parse(SETTINGS).unwrap();
    let passwd =
        "triage:x:1002:1002::/home/triage:/bin/bash\nworker:x:1003:1003::/w:/bin/sh\nbuilder:x:1004:1004::/b:/bin/sh\n";
    let subuid = "triage:100000:65536\n1003:165536:65536\n";
    let ids = crate::firewall::ids(&s, passwd, subuid).unwrap();
    let f = crate::firewall::script(&s, "egress.yaml", &ids);
    // The role's own uid, and the subordinate uids it can run processes as (rootless podman).
    let triage = "meta skuid { 1002, 100000-165535 }";
    assert!(f.contains(&format!("{triage} tcp dport 443 redirect to :3129")), "{f}");
    assert!(f.contains(&format!("{triage} meta l4proto {{ tcp, udp }} th dport 53 redirect to :3153")), "{f}");
    assert!(f.contains(&format!("{triage} meta l4proto tcp reject with tcp reset")), "{f}");
    let workers = "meta skuid { 1003, 165536-231071, 1004 }";
    assert!(f.contains(&format!("{workers} ct state new limit rate")), "log mode logs, never refuses: {f}");
    assert!(!f.contains(&format!("{workers} reject")), "{f}");
    assert!(crate::firewall::ids(&s, "triage:x:1002:1002::/h:/bin/sh\n", "").unwrap_err().contains("worker"));
    assert!(
        f.contains("table inet fleetlayer_egress\ndelete table inet fleetlayer_egress\n"),
        "applying twice replaces it"
    );
    assert!(crate::firewall::script(&settings::parse("").unwrap(), "x", &Default::default()).contains("No roles"));
    // The proxy counts a subordinate uid as its user, and refuses a uid nobody owns.
    let mut users = crate::procnet::Users::from_files(passwd, subuid);
    assert_eq!(users.name(100_005).as_deref(), Some("triage"));
    assert_eq!(users.name(170_000).as_deref(), Some("worker"));
    assert_eq!(users.lookup_only(400_000), None);
}

#[test]
fn whys_are_one_line_and_long_enough() {
    assert!(crate::check_why("the vendor's API for the invoice import").is_ok());
    assert!(crate::check_why("too short").is_err());
    let looked = |reason: &str| fleetlayer_protocol::link::AccessDecision {
        allowed: false,
        rules: vec![],
        reason: reason.into(),
    };
    assert_eq!(
        crate::engine::answer_from(&looked("not asked yet: -@x's own node files the request")),
        Answer::NotAsked
    );
    assert_eq!(
        crate::engine::answer_from(&looked("waiting for the person to approve request ap_7")),
        Answer::Waiting(Some("ap_7".into()))
    );
    assert!(crate::check_why("two lines\nof text, long enough").is_err());
    assert_eq!(
        crate::state::approval_id("waiting for the person to approve request ap_01ABC. Try"),
        Some("ap_01ABC".into())
    );
}

// From the review of 9e4b441: each of these failed before its fix.

#[test]
fn a_connection_s_owner_is_its_established_socket_never_a_time_wait_row() {
    // A closed connection from the same port (TIME_WAIT, uid 0, inode 0) and the new one (ESTABLISHED, uid 1002).
    let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
               0: 2507050A:D431 0101A8C0:01BB 06 00000000:00000000 00:00000000 00000000     0        0 0 1\n   \
               1: 2507050A:D431 0100007F:0C38 01 00000000:00000000 00:00000000 00000000  1002        0 9 1\n";
    let peer: SocketAddr = "10.5.7.37:54321".parse().unwrap();
    assert_eq!(crate::procnet::tcp_owner(&[tcp.to_string()], peer).unwrap().uid, 1002);
    let only_closed = tcp.lines().take(2).collect::<Vec<_>>().join("\n");
    assert_eq!(crate::procnet::tcp_owner(&[only_closed], peer), None, "a TIME_WAIT row is nobody");
    // Two live sockets on that port with different owners: can't tell, so nobody.
    let two = format!(
        "{tcp}   2: 2507050A:D431 0202A8C0:01BB 01 00000000:00000000 00:00000000 00000000     0        0 7 1\n"
    );
    assert_eq!(crate::procnet::tcp_owner(&[two], peer), None);
}

#[tokio::test]
async fn plain_http_forwards_one_request_per_connection() {
    let up = echo().await;
    let text = format!(
        "mode: enforce\nroles:\n  triage:\n    users: [triage]\n    networks: [127.0.0.0/8]\n    hosts: [\"localhost:{}\"]\n",
        up.port()
    );
    let (e, _dir) = engine(&text);
    let http = serve(e.clone(), crate::proxy::Kind::Http, "triage", up).await;
    // A second request with another Host, pipelined after an allowed one, must not reach the first host.
    let reqs = format!(
        "GET /a HTTP/1.1\r\nHost: localhost:{}\r\nConnection: keep-alive\r\n\r\nGET /b HTTP/1.1\r\nHost: pastebin.com\r\n\r\n",
        up.port()
    );
    let mut s = TcpStream::connect(http).await.unwrap();
    s.write_all(reqs.as_bytes()).await.unwrap();
    let mut got = vec![];
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), s.read_to_end(&mut got)).await;
    let got = String::from_utf8_lossy(&got);
    assert!(got.contains("GET /a"), "{got}");
    assert!(!got.contains("pastebin.com"), "the second request reached the first host: {got}");
    assert!(got.contains("Connection: close") && !got.contains("keep-alive"), "{got}");
    // A body is forwarded by its Content-Length, and nothing after it.
    let req = format!(
        "POST /c HTTP/1.1\r\nHost: localhost:{}\r\nContent-Length: 5\r\n\r\nhelloGET /d HTTP/1.1\r\n\r\n",
        up.port()
    );
    let mut s = TcpStream::connect(http).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut got = vec![];
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), s.read_to_end(&mut got)).await;
    let got = String::from_utf8_lossy(&got);
    assert!(got.contains("hello") && !got.contains("GET /d"), "{got}");
    // A chunked body can't be bounded without parsing it: refused, over plain HTTP.
    let req =
        format!("POST /e HTTP/1.1\r\nHost: localhost:{}\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n", up.port());
    let mut s = TcpStream::connect(http).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    assert!(read_all(&mut s).await.starts_with("HTTP/1.1 411"));
}

#[tokio::test]
async fn private_addresses_only_through_the_role_s_networks() {
    let (e, _d) = engine(SETTINGS);
    e.state.lock().await.roles.entry("triage".into()).or_default().approved.insert(
        "host:docs.attacker.dev".into(),
        crate::state::Approved {
            target: "host:docs.attacker.dev".into(),
            checked_at: jiff::Timestamp::now(),
            used_at: None,
        },
    );
    let approved = e.decide(Some("triage"), &t("docs.attacker.dev", 443)).await;
    assert!(approved.go);
    assert!(!approved.address_ok("169.254.169.254".parse().unwrap()), "an approved host pointed at cloud metadata");
    assert!(!approved.address_ok("192.168.1.10".parse().unwrap()));
    let exact = e.decide(Some("triage"), &t("api.example.com", 443)).await;
    assert!(!exact.address_ok("127.0.0.1".parse().unwrap()), "a listed name pointed at loopback");
    assert!(exact.address_ok("10.5.7.50".parse().unwrap()), "the role's networks");
    assert!(exact.address_ok("1.1.1.1".parse().unwrap()));
}
