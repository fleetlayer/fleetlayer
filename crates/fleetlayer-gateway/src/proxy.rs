//! The proxy: a request for `<name>.<domain>` goes to that service's backend, over HTTP/1.1, with bodies streamed both
//! ways (server-sent events work) and upgrades passed through (WebSocket). The Host header is kept, so a service that
//! serves several hosts (pages' apps) sees which one was asked for; `X-Forwarded-For/-Host/-Proto` are set.

use std::net::SocketAddr;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode, Uri, header};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};

pub type HttpClient = Client<HttpConnector, Body>;

pub fn client() -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(std::time::Duration::from_secs(5)));
    Client::builder(TokioExecutor::new()).build(connector)
}

/// Headers that belong to one connection, never forwarded (RFC 9110 7.6.1), except `Connection`/`Upgrade` on an
/// upgrade request.
const HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn strip_hop(h: &mut HeaderMap) {
    // Headers the Connection header names are hop-by-hop too.
    let named: Vec<HeaderName> = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|n| HeaderName::from_bytes(n.trim().as_bytes()).ok())
        .collect();
    for n in named {
        h.remove(n);
    }
    for n in HOP {
        h.remove(n);
    }
}

fn upgrade_of(h: &HeaderMap) -> Option<HeaderValue> {
    let wants = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade")));
    if wants { h.get(header::UPGRADE).cloned() } else { None }
}

/// Forward one request to `backend` (`http://127.0.0.1:7670`).
pub async fn forward(
    client: &HttpClient,
    backend: &str,
    peer: SocketAddr,
    mut req: Request<Body>,
) -> Result<Response<Body>, String> {
    let base: Uri = backend.parse().map_err(|e| format!("bad backend address {backend}: {e}"))?;
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str()).to_string();
    let mut parts = base.into_parts();
    parts.path_and_query = Some(path.parse().map_err(|e| format!("{e}"))?);
    let uri = Uri::from_parts(parts).map_err(|e| format!("{e}"))?;

    let upgrade = upgrade_of(req.headers());
    let client_upgrade = upgrade.as_ref().map(|_| hyper::upgrade::on(&mut req));
    let (mut head, body) = req.into_parts();
    let host = head.headers.get(header::HOST).cloned();
    strip_hop(&mut head.headers);
    if let Some(u) = &upgrade {
        head.headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        head.headers.insert(header::UPGRADE, u.clone());
    }
    let ip = peer.ip().to_string();
    let xff = match head.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(prev) => format!("{prev}, {ip}"),
        None => ip,
    };
    head.headers.insert("x-forwarded-for", HeaderValue::from_str(&xff).map_err(|e| e.to_string())?);
    head.headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    if let Some(h) = host {
        head.headers.insert("x-forwarded-host", h);
    }
    head.uri = uri;
    head.version = axum::http::Version::HTTP_11;
    let out = Request::from_parts(head, body);

    let mut resp = client.request(out).await.map_err(|e| {
        use std::error::Error;
        let mut msg = e.to_string();
        let mut src = e.source();
        while let Some(s) = src {
            msg = format!("{msg}: {s}");
            src = s.source();
        }
        msg
    })?;

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(client_upgrade) = client_upgrade
    {
        let backend_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            match tokio::try_join!(client_upgrade, backend_upgrade) {
                Ok((a, b)) => {
                    let mut a = TokioIo::new(a);
                    let mut b = TokioIo::new(b);
                    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                }
                Err(e) => tracing::debug!("upgrade failed: {e}"),
            }
        });
        let (parts, _) = resp.into_parts();
        return Ok(Response::from_parts(parts, Body::empty()));
    }

    let (mut parts, body) = resp.into_parts();
    strip_hop(&mut parts.headers);
    Ok(Response::from_parts(parts, Body::new(body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_go() {
        let mut h = HeaderMap::new();
        h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive, x-secret"));
        h.insert("x-secret", HeaderValue::from_static("1"));
        h.insert("keep-alive", HeaderValue::from_static("5"));
        h.insert("x-kept", HeaderValue::from_static("1"));
        assert!(upgrade_of(&h).is_none());
        strip_hop(&mut h);
        assert_eq!(h.len(), 1);
        assert!(h.contains_key("x-kept"));
    }

    #[test]
    fn upgrades_are_seen() {
        let mut h = HeaderMap::new();
        h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive, Upgrade"));
        h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        assert_eq!(upgrade_of(&h).unwrap(), "websocket");
    }
}
