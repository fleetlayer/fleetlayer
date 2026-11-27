//! The gateway end to end: a backend behind it, reached by Host header, with streaming, upgrades, headers, the
//! status page, a backend that's down, and heartbeats.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::Config;
use crate::registry::Registry;
use crate::{App, monitor, proxy};

async fn backend() -> SocketAddr {
    async fn echo_headers(headers: HeaderMap) -> String {
        let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
        format!(
            "host={} xff={} xfh={} xfp={}",
            h("host"),
            h("x-forwarded-for"),
            h("x-forwarded-host"),
            h("x-forwarded-proto")
        )
    }
    async fn stream() -> impl IntoResponse {
        let s = futures::stream::unfold(0, |i| async move {
            if i == 3 {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Some((Ok::<_, std::io::Error>(format!("data: {i}\n\n")), i + 1))
        });
        ([("content-type", "text/event-stream")], Body::from_stream(s))
    }
    async fn upgrade(mut req: Request<Body>) -> impl IntoResponse {
        let on = hyper::upgrade::on(&mut req);
        tokio::spawn(async move {
            if let Ok(up) = on.await {
                let mut io = hyper_util::rt::TokioIo::new(up);
                let mut buf = [0u8; 5];
                if io.read_exact(&mut buf).await.is_ok() {
                    let _ = io.write_all(&buf).await;
                }
            }
        });
        (StatusCode::SWITCHING_PROTOCOLS, [("connection", "upgrade"), ("upgrade", "echo")])
    }
    let app = Router::new()
        .route("/headers", get(echo_headers))
        .route("/stream", get(stream))
        .route("/echo", get(upgrade))
        .route("/healthz", get(|| async { "ok" }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

async fn gateway(yaml: &str, dir: &std::path::Path) -> (SocketAddr, Arc<App>) {
    let mut cfg: Config = yaml_serde::from_str(yaml).unwrap();
    cfg.node = false;
    cfg.alerts.desktop = false;
    cfg.state_dir = Some(dir.to_path_buf());
    cfg.check().unwrap();
    let reg = Registry::new(cfg);
    let monitor = monitor::Monitor::new(reg.clone());
    let app = Arc::new(App { reg, monitor, client: proxy::client(), config_path: dir.join("gateway.yaml") });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let router = crate::router(app.clone());
    tokio::spawn(
        async move { axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap() },
    );
    (addr, app)
}

/// A raw HTTP/1.1 exchange, so the Host header is exactly what we say.
async fn raw(addr: SocketAddr, request: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(request.as_bytes()).await.unwrap();
    let mut out = vec![];
    s.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

fn get_req(host: &str, path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
}

#[tokio::test]
async fn proxies_by_host_with_headers_and_streams() {
    let b = backend().await;
    let dir = tempfile::tempdir().unwrap();
    let (gw, _app) = gateway(
        &format!("services:\n  hub: {{ url: 'http://{b}', health: /healthz }}\n  pages: {{ url: 'http://{b}', hosts: ['*--*.apps'] }}\n"),
        dir.path(),
    )
    .await;

    let out = raw(gw, &get_req("hub.localhost:7680", "/headers")).await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.contains("host=hub.localhost:7680 xff=127.0.0.1 xfh=hub.localhost:7680 xfp=http"), "{out}");

    let out = raw(gw, &get_req("recipes--personal.apps.localhost:7680", "/headers")).await;
    assert!(out.contains("host=recipes--personal.apps.localhost:7680"), "{out}");

    let out = raw(gw, &get_req("pages.localhost", "/stream")).await;
    assert!(out.contains("data: 0") && out.contains("data: 2"), "{out}");
}

#[tokio::test]
async fn upgrades_pass_through() {
    let b = backend().await;
    let dir = tempfile::tempdir().unwrap();
    let (gw, _app) = gateway(&format!("services:\n  hub: {{ url: 'http://{b}' }}\n"), dir.path()).await;
    let mut s = tokio::net::TcpStream::connect(gw).await.unwrap();
    s.write_all(b"GET /echo HTTP/1.1\r\nHost: hub.localhost\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n")
        .await
        .unwrap();
    let mut head = vec![];
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8];
        s.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    s.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    tokio::time::timeout(std::time::Duration::from_secs(5), s.read_exact(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf, b"hello");
}

#[tokio::test]
async fn status_page_unknown_hosts_and_down_backends() {
    let dir = tempfile::tempdir().unwrap();
    // A port nothing listens on.
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let (gw, _app) =
        gateway(&format!("services:\n  gone: {{ url: 'http://{dead}', health: /healthz }}\n"), dir.path()).await;

    let out = raw(gw, &get_req("fleetlayer.localhost:7680", "/")).await;
    assert!(out.starts_with("HTTP/1.1 200") && out.contains("Fleetlayer is running on this machine"), "{out}");
    assert!(out.contains("http://gone.localhost:7680"), "{out}");
    let out = raw(gw, &get_req("localhost:7680", "/healthz")).await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    let out = raw(gw, &get_req("127.0.0.1:7680", "/")).await;
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");

    let out = raw(gw, &get_req("nope.localhost:7680", "/")).await;
    assert!(out.starts_with("HTTP/1.1 404") && out.contains("Nothing called <b>nope</b>"), "{out}");

    let out = raw(gw, &get_req("gone.localhost:7680", "/")).await;
    assert!(out.starts_with("HTTP/1.1 502") && out.contains("gone isn't answering"), "{out}");

    let out = raw(gw, &get_req("example.com", "/")).await;
    assert!(out.starts_with("HTTP/1.1 421"), "{out}");
}

#[tokio::test]
async fn health_checks_and_heartbeats_change_state() {
    let b = backend().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hb-token"), "s3cret\n").unwrap();
    let (gw, app) = gateway(
        &format!(
            "services:\n  hub: {{ url: 'http://{b}', health: /healthz }}\nheartbeats:\n  sync: {{ every: 60, token_file: '{}' }}\n",
            dir.path().join("hb-token").display()
        ),
        dir.path(),
    )
    .await;
    app.monitor.tick().await;
    {
        let s = app.monitor.state.lock().await;
        assert_eq!(s.checks["service:hub"].2.up, Some(true));
    }

    let push = |q: &str, token: &str| {
        format!(
            "POST /api/v1/endpoints/fleetlayer_sync/external?{q} HTTP/1.1\r\nHost: fleetlayer.localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    };
    assert!(raw(gw, &push("success=true", "wrong")).await.starts_with("HTTP/1.1 401"));
    assert!(raw(gw, &push("success=true", "s3cret")).await.starts_with("HTTP/1.1 200"));
    assert_eq!(app.monitor.state.lock().await.checks["heartbeat:sync"].2.up, Some(true));
    assert!(raw(gw, &push("success=false&error=lost%20the+link", "s3cret")).await.starts_with("HTTP/1.1 200"));
    {
        let s = app.monitor.state.lock().await;
        let c = &s.checks["heartbeat:sync"].2;
        assert_eq!(c.up, Some(false));
        assert_eq!(c.last_error.as_deref(), Some("lost the link"));
    }
    let unknown = push("success=true", "s3cret").replace("fleetlayer_sync", "other");
    assert!(raw(gw, &unknown).await.starts_with("HTTP/1.1 404"));

    let out = raw(gw, &get_req("fleetlayer.localhost", "/api/status")).await;
    assert!(out.contains("\"name\":\"hub\"") && out.contains("\"heartbeat\""), "{out}");
    // State survives a restart.
    assert!(dir.path().join("checks.json").exists());
}
