//! The MCP gate: a node that shares an MCP server checks each of its tool calls with the control plane before the
//! server sees it (`check_calls: true` on the tool). Each call is named as one or more targets, decided as verb `call`
//! on `tool:<tool>/<target>@<node>` by the policy file (allow, ask, deny), and recorded in the audit log with its
//! caller. `docs/access-control.md` ("The MCP gate") describes it; `docs/guides/executor.md` puts Executor behind it.
//!
//! A target is the MCP tool's name, and for tools with argument rules (`call_targets:`), one more part per value the
//! rule picks out of the arguments: Executor's `call` with `{calls: [{path: "tools.google.gmail.send"}]}` and the rule
//! `call: ["/calls/*/path"]` is the target `call/tools.google.gmail.send`.

use std::collections::BTreeMap;
use std::time::Duration;

use fleetlayer_protocol::link::{self as l, ToolCall, ToolCallDecision};
use serde_json::Value;
use sha2::Digest;

use crate::Node;

/// How long the gate holds a call that waits for the person, by default, before answering the agent that it's still
/// waiting. An MCP client gives up on a call after a while of its own, so this stays under a few minutes.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(120);
/// How often to ask again while waiting.
const POLL: Duration = Duration::from_secs(5);
/// The longest detail the person is shown. A call that won't fit isn't put to them at all (the control plane refuses
/// to ask with no detail): they'd approve arguments they never saw. The control plane takes up to 8192 bytes.
const DETAIL_MAX: usize = 8000;

/// Methods that pass without a check: they only set up the connection or list what's there. Every other request is
/// checked as `rpc/<method>` (`resources/read`, `prompts/get`...), and `tools/call` by its tool and arguments.
const UNCHECKED: &[&str] = &["initialize", "ping", "tools/list"];

/// One message the gate checks before the server sees it.
pub struct Gated {
    /// The tool's name, or `rpc/<method>`.
    pub label: String,
    /// The targets to decide, or why the gate can't name them.
    pub targets: Result<Vec<String>, String>,
    pub args: Value,
    pub id: Value,
}

/// Whether a message needs checking, and as what. None for replies to the server, notifications and the methods in
/// [`UNCHECKED`].
pub fn gated(text: &str, rules: &BTreeMap<String, Vec<String>>) -> Option<Gated> {
    let v: Value = serde_json::from_str(text).ok()?;
    let method = v.get("method")?.as_str()?;
    if method.starts_with("notifications/") || UNCHECKED.contains(&method) {
        return None;
    }
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    if method == "tools/call" {
        let name = v["params"]["name"].as_str().unwrap_or_default().to_string();
        let args = v["params"]["arguments"].clone();
        return Some(Gated { targets: targets(&name, &args, rules), label: name, args, id });
    }
    let label = format!("rpc/{method}");
    let targets = match clean(method, VALUE_CHARS) {
        Some(_) => Ok(vec![label.clone()]),
        None => Err(format!("the method name {method:?} can't be checked")),
    };
    Some(Gated { label, targets, args: v["params"].clone(), id })
}

/// The line to pass on: the message as the gate parsed it. A duplicate key then means the same to the server as it did
/// to the checks (serde_json keeps the last one; a server might keep the first).
pub fn reencode(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Object(_)) => v.to_string(),
        _ => text.to_string(),
    }
}

/// The targets a call names: its tool name, then, for each argument rule of that tool, `<name>/<value>` for each
/// value the rule picks out. A rule that picks out nothing, or a value that isn't a short string, refuses the call:
/// the gate never lets a call through as less than it is.
pub fn targets(name: &str, args: &Value, rules: &BTreeMap<String, Vec<String>>) -> Result<Vec<String>, String> {
    let name = clean(name, NAME_CHARS).ok_or_else(|| format!("the tool name {name:?} can't be checked"))?;
    let Some(pointers) = rules.get(&name) else { return Ok(vec![name]) };
    let mut out = vec![];
    for p in pointers {
        let found = pick(args, p).unwrap_or_default();
        if found.is_empty() {
            return Err(format!("this {name} call has nothing at {p}, so the gate can't tell what it does"));
        }
        for v in found {
            let s = v.as_str().and_then(|s| clean(s, VALUE_CHARS)).ok_or_else(|| {
                format!(
                    "this {name} call has something at {p} other than a short string of letters, digits and . _ / : -, \
                     so the gate can't check it"
                )
            })?;
            let t = format!("{name}/{s}");
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    Ok(out)
}

/// The characters of a tool name (MCP's own rule), and of a value picked out of the arguments.
const NAME_CHARS: &str = "._-";
const VALUE_CHARS: &str = "._-/:";

/// A name or value as part of a target: 1 to 200 ASCII letters, digits and the `extra` characters. Nothing a server
/// might read differently (spaces, look-alike letters, an `@` that would end the target). None when it isn't.
fn clean(s: &str, extra: &str) -> Option<String> {
    (!s.is_empty() && s.len() <= 200 && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c)))
        .then(|| s.to_string())
}

/// The values at a JSON pointer, where a `*` segment means each item of an array, or the value itself when it isn't
/// an array (Executor's `calls` is one call or a list of them). None when any branch lacks what the pointer names, so
/// one call in a list without a path can't hide behind the others.
pub fn pick<'a>(v: &'a Value, pointer: &str) -> Option<Vec<&'a Value>> {
    let mut here = vec![v];
    for seg in pointer.split('/').skip(1) {
        let seg = seg.replace("~1", "/").replace("~0", "~");
        let mut next = vec![];
        for h in here {
            match (seg.as_str(), h) {
                ("*", Value::Array(items)) => next.extend(items.iter()),
                ("*", other) => next.push(other),
                (key, Value::Object(m)) => next.push(m.get(key)?),
                (idx, Value::Array(items)) => next.push(idx.parse::<usize>().ok().and_then(|i| items.get(i))?),
                _ => return None,
            }
        }
        here = next;
    }
    Some(here)
}

/// A value with every object's keys in order, whatever order they came in (and whether or not serde_json keeps the
/// order it read, which a feature turned on anywhere in the build decides).
fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), sorted(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// A hash of a call's arguments, the same for the same arguments whatever order their keys came in: what an approval
/// `for: once` is bound to.
pub fn args_hash(name: &str, args: &Value) -> String {
    let text = sorted(&serde_json::json!({"name": name, "arguments": args})).to_string();
    hex::encode(&sha2::Sha256::digest(text.as_bytes())[..16])
}

/// What the person sees when asked: the tool and its arguments, cut short. For a program (Executor's `execute`), the
/// code as it is, then the tool paths it names, which help the person read it but don't bind it to anything.
pub fn detail(name: &str, args: &Value) -> Option<String> {
    let out = match args.get("code").and_then(Value::as_str) {
        Some(code) => {
            let paths = paths_named(code);
            let named = if paths.is_empty() { "none".to_string() } else { paths.join(", ") };
            format!("{name}, a program. Tool paths it names (it may reach others): {named}\n\n{code}")
        }
        None => format!("{name} {}", serde_json::to_string_pretty(args).unwrap_or_default()),
    };
    let out = visible(&out);
    (out.len() <= DETAIL_MAX).then_some(out)
}

/// Characters that change how text looks without showing themselves (bidirectional overrides, zero-width characters,
/// soft hyphens, control characters other than newline and tab) written out as `<U+XXXX>`, so what the person reads
/// is what the call holds.
pub fn visible(text: &str) -> String {
    let hidden = |c: char| {
        (c.is_control() && c != '\n' && c != '\t')
            || matches!(c as u32, 0x00AD | 0x061C | 0x115F | 0x1160 | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E
                | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 | 0xE0000..=0xE007F)
    };
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if hidden(c) {
            out.push_str(&format!("<U+{:04X}>", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `tools.<a>.<b>…` paths written out in a program's text. Only a reading aid: a program can build a path at run time.
pub fn paths_named(code: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut rest = code;
    while let Some(i) = rest.find("tools.") {
        let before_ok = i == 0 || !rest[..i].ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.');
        let tail = &rest[i..];
        let end = tail.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '-')).unwrap_or(tail.len());
        let path = tail[..end].trim_end_matches('.');
        if before_ok && path.len() > "tools.".len() && !out.iter().any(|p| p == path) && out.len() < 30 {
            out.push(path.to_string());
        }
        rest = &rest[i + "tools.".len()..];
    }
    out
}

/// The control plane's answer on one call, after waiting up to `wait` for the person when it asks them.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Allowed,
    /// The text to answer the agent with.
    Refused(String),
}

/// Ask the control plane about a call on stream `stream_id`; while it waits for the person, ask again every few
/// seconds until `wait` is up. Fails closed: no answer is a refusal.
pub async fn decide(node: &Node, stream_id: &str, call: &ToolCall, wait: Duration) -> Verdict {
    let started = tokio::time::Instant::now();
    let unreachable = |e: String| {
        Verdict::Refused(format!(
            "{} couldn't check this call with the control plane ({e}), so it didn't make it. Try again in a minute; if \
             it keeps failing, tell your person.",
            node.name
        ))
    };
    let mut d = match ask(node, call).await {
        Ok(d) => d,
        Err(e) => return unreachable(e),
    };
    loop {
        match d.decision.as_str() {
            "allowed" => return Verdict::Allowed,
            "pending" if started.elapsed() + POLL < wait => {
                tracing::info!(stream = %&stream_id[..stream_id.len().min(8)], approval = ?d.approval, "a tool call waits for approval");
                tokio::time::sleep(POLL).await;
                // Only look at the request while it waits, so the wait writes nothing in the audit log; once it's
                // approved, ask again in full (which spends a `for: once` approval).
                let peek = ToolCall { peek: d.approval.clone(), ..call.clone() };
                let looked = match ask(node, &peek).await {
                    Ok(p) => p,
                    Err(e) => return unreachable(e),
                };
                d = match looked.decision.as_str() {
                    "ready" => match ask(node, call).await {
                        Ok(full) => full,
                        Err(e) => return unreachable(e),
                    },
                    _ => looked,
                };
            }
            "pending" => {
                let id = d.approval.unwrap_or_default();
                return Verdict::Refused(format!(
                    "This call needs your person's approval (request {id}), and they haven't answered yet. Tell them \
                     what you're trying to do and that request {id} is waiting, then make the same call again with \
                     the same arguments once they've approved it. ({})",
                    d.reason
                ));
            }
            _ => return Verdict::Refused(d.reason),
        }
    }
}

async fn ask(node: &Node, call: &ToolCall) -> Result<ToolCallDecision, String> {
    let peer = node.link.peer().await.ok_or_else(|| "not linked to it now".to_string())?;
    peer.request_within(l::method::TOOL_CALL, call, Duration::from_secs(20)).await.map_err(|e| e.message)
}

/// Redact the secrets delivered to this node from one line passing through the gate, in either direction. `context`
/// says where it was going, for the alert; never a value.
pub fn scrub<'a>(node: &Node, text: &'a str, context: &str) -> std::borrow::Cow<'a, str> {
    // The node's scrubber (`crate::secrets`): exact matches and their encoded forms, and on a hit it raises the alert
    // that says to rotate.
    crate::secrets::scrub(node, text, context)
}

/// The latest check of each remote server this node shares (`crate::mcp_http::probe`), for the node's health.
#[derive(Default)]
pub struct Health {
    failing: tokio::sync::Mutex<BTreeMap<String, String>>,
}

impl Health {
    /// The problems to report: each remote tool whose last check failed.
    pub async fn problems(&self) -> Vec<String> {
        let f = self.failing.lock().await;
        f.iter().map(|(t, why)| format!("the server behind shared tool {t} isn't working ({why})")).collect()
    }
}

/// How often each remote server is checked.
const PROBE_EVERY: Duration = Duration::from_secs(300);

/// Check every remote server this node shares, now and every five minutes: initialize and list its tools with the
/// node's credentials, so a revoked token or a server that's up but broken fails the node's health (and its
/// heartbeat), not only one that's down.
pub async fn watch(node: std::sync::Arc<Node>) {
    let remote: Vec<_> = node.tools.iter().filter(|t| t.url.is_some()).cloned().collect();
    if remote.is_empty() {
        return;
    }
    let mut tick = tokio::time::interval(PROBE_EVERY);
    loop {
        tick.tick().await;
        for t in &remote {
            let result = match crate::tools::upstream(&node, t) {
                Ok(up) => crate::mcp_http::probe(&up).await.map(|_| ()),
                Err(e) => Err(e),
            };
            let mut f = node.gate.failing.lock().await;
            match result {
                Ok(()) => {
                    if f.remove(&t.name).is_some() {
                        tracing::info!(tool = %t.name, "the shared tool's server works again");
                    }
                }
                Err(why) => {
                    tracing::warn!(tool = %t.name, "the shared tool's server isn't working: {why}");
                    f.insert(t.name.clone(), why);
                }
            }
        }
    }
}

/// An MCP error result for a refused call, with the caller's request ID.
pub fn refusal(id: &Value, text: &str) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"isError": true, "content": [{"type": "text", "text": text}]}})
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn executor() -> BTreeMap<String, Vec<String>> {
        BTreeMap::from([("call".to_string(), vec!["/calls/*/path".to_string()])])
    }

    #[test]
    fn executor_calls_are_named_by_their_paths() {
        let one = json!({"calls": {"path": "tools.google.gmail.send", "input": {}}});
        assert_eq!(targets("call", &one, &executor()).unwrap(), ["call/tools.google.gmail.send"]);
        let many = json!({"calls": [{"path": "tools.a.read", "input": {}}, {"path": "tools.b.write", "input": {}},
            {"path": "tools.a.read", "input": {"x": 1}}]});
        assert_eq!(targets("call", &many, &executor()).unwrap(), ["call/tools.a.read", "call/tools.b.write"]);
        assert_eq!(targets("search", &json!({"query": "mail"}), &executor()).unwrap(), ["search"]);
        assert_eq!(targets("execute", &json!({"code": "return 1"}), &executor()).unwrap(), ["execute"]);
    }

    #[test]
    fn a_call_the_gate_cant_read_is_refused_not_passed_as_less() {
        let r = executor();
        assert!(targets("call", &json!({}), &r).is_err(), "no calls");
        assert!(targets("call", &json!({"calls": []}), &r).is_err(), "an empty list");
        assert!(targets("call", &json!({"calls": [{"path": 7}]}), &r).is_err(), "not a string");
        assert!(targets("call", &json!({"calls": [{"path": "tools.a"}, {"input": {}}]}), &r).is_err(), "one missing");
        assert!(targets("call", &json!({"calls": {"path": "tools.x@evil"}}), &r).is_err(), "an @ would end the target");
        assert!(targets("call", &json!({"calls": {"path": "a\nb"}}), &r).is_err());
        assert!(targets("bad@name", &json!({}), &r).is_err());
        assert!(targets("call", &json!({"calls": {"path": "x".repeat(201)}}), &r).is_err());
        // Only plain ASCII: nothing a server might normalise into another path.
        assert!(targets("call", &json!({"calls": {"path": "tools.google.gmail. send"}}), &r).is_err(), "a space");
        assert!(targets("call", &json!({"calls": {"path": "tools.google.gmail.ｓend"}}), &r).is_err(), "full width");
        assert!(targets("call", &json!({"calls": {"path": "tools.google.gmail.send\u{200b}"}}), &r).is_err());
        assert!(targets("c all", &json!({}), &r).is_err());
        assert!(targets("call", &json!({"calls": {"path": "tools.google.gmail.readThread"}}), &r).is_ok());
    }

    #[test]
    fn every_request_but_setup_and_listing_is_checked() {
        let r = executor();
        let g = |t: &str| gated(t, &r).map(|g| (g.label, g.targets));
        assert!(g(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).is_none());
        assert!(g(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).is_none());
        assert!(g(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        assert!(g(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#).is_none(), "a reply to the server");
        assert_eq!(
            g(r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"x"}}"#),
            Some(("rpc/resources/read".into(), Ok(vec!["rpc/resources/read".into()])))
        );
        assert_eq!(
            g(
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"call","arguments":{"calls":{"path":"tools.a"}}}}"#
            ),
            Some(("call".into(), Ok(vec!["call/tools.a".into()])))
        );
        assert!(g(r#"{"jsonrpc":"2.0","id":6,"method":"weird method"}"#).unwrap().1.is_err());
    }

    #[test]
    fn the_server_gets_the_message_the_checks_read() {
        let dup = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"drop","name":"search"}}"#;
        let g = gated(dup, &BTreeMap::new()).unwrap();
        assert_eq!(g.label, "search");
        let sent: Value = serde_json::from_str(&reencode(dup)).unwrap();
        assert_eq!(sent["params"]["name"], "search");
        assert_eq!(reencode(dup).matches("\"name\"").count(), 1);
    }

    #[test]
    fn pointers_take_indexes_keys_and_escapes() {
        let v = json!({"a/b": [{"c": "one"}, {"c": "two"}]});
        assert_eq!(pick(&v, "/a~1b/1/c"), Some(vec![&json!("two")]));
        assert_eq!(pick(&v, "/a~1b/*/c").map(|v| v.len()), Some(2));
        assert_eq!(pick(&v, "/missing/*"), None);
        assert_eq!(pick(&json!({"a": [{"c": 1}, {"d": 2}]}), "/a/*/c"), None, "one item without it");
    }

    #[test]
    fn the_hash_ignores_key_order_and_changes_with_any_argument() {
        let a = args_hash("call", &json!({"x": 1, "y": {"b": 2, "a": 1}}));
        let b = args_hash("call", &serde_json::from_str(r#"{"y": {"a": 1, "b": 2}, "x": 1}"#).unwrap());
        assert_eq!(a, b);
        assert_ne!(a, args_hash("call", &json!({"x": 2, "y": {"b": 2, "a": 1}})));
        assert_ne!(a, args_hash("other", &json!({"x": 1, "y": {"b": 2, "a": 1}})));
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn a_program_shows_its_code_and_the_paths_it_names() {
        let code = "const t = await tools.google.gmail.search({q: 'x'});\nreturn tools.slack.post(t); // mytools.no";
        assert_eq!(paths_named(code), ["tools.google.gmail.search", "tools.slack.post"]);
        let d = detail("execute", &json!({"code": code})).unwrap();
        assert!(d.contains("tools.google.gmail.search, tools.slack.post") && d.ends_with("// mytools.no"), "{d}");
        // Too long to show whole: no detail, never a part of one.
        assert_eq!(detail("call", &json!({"calls": {"path": "tools.a", "input": {"body": "é".repeat(5000)}}})), None);
        // Text that hides itself is written out.
        let d = detail("call", &json!({"to": "sam\u{202e}moc.live", "x": "a\u{200b}b"})).unwrap();
        assert!(d.contains("sam<U+202E>moc.live") && d.contains("a<U+200B>b"), "{d}");
    }
}
