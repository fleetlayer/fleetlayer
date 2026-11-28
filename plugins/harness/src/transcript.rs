//! Transcripts in one shape. Each harness keeps its conversation its own way (pi and Gemini CLI as JSONL of their own,
//! OpenCode in SQLite behind `opencode export`, Cursor as JSONL); the node's readers, the phone's Conversation view and
//! `fleetlayer transcript --types user,assistant` read Claude Code's JSONL. So each plugin writes a copy in the part of
//! that shape they use, the format `fleetlayer-jsonl`:
//!
//! ```json
//! {"type":"user","timestamp":"…","sessionId":"…","message":{"role":"user","content":[{"type":"text","text":"…"}]}}
//! {"type":"assistant","timestamp":"…","sessionId":"…","message":{"role":"assistant","content":[
//!   {"type":"text","text":"…"},{"type":"tool_use","id":"…","name":"bash","input":{…}}]}}
//! {"type":"user","timestamp":"…","sessionId":"…","message":{"role":"user","content":[
//!   {"type":"tool_result","tool_use_id":"…","content":"…","is_error":false}]}}
//! ```

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{Value, json};

pub const FORMAT: &str = "fleetlayer-jsonl";

/// The most of one tool result kept, in bytes: the copy is for reading the conversation, not replaying it.
const RESULT_MAX: usize = 16 * 1024;

fn line(kind: &str, session: &str, timestamp: Option<Value>, content: Vec<Value>) -> Value {
    let mut v = json!({"type": kind, "sessionId": session, "message": {"role": kind, "content": content}});
    if let Some(t) = timestamp.filter(|t| !t.is_null()) {
        v["timestamp"] = t;
    }
    v
}

fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn cut(s: String) -> String {
    if s.len() <= RESULT_MAX {
        return s;
    }
    let mut end = RESULT_MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[cut]", &s[..end])
}

/// The text in a content value: a string, or a list of `{type: "text", text}` (other items left out).
fn texts(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(s.as_str()),
                _ => i.get("text").and_then(Value::as_str),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn ms_to_iso(ms: &Value) -> Option<Value> {
    let ms = ms.as_i64()?;
    jiff::Timestamp::from_millisecond(ms).ok().map(|t| Value::String(t.to_string()))
}

/// pi's session file: `{"type":"message","timestamp","message":{"role":"user|assistant|toolResult|…","content":[…]}}`.
pub fn from_pi(record: &Value, session: &str) -> Vec<Value> {
    if record["type"] != "message" {
        return vec![];
    }
    let m = &record["message"];
    let ts = record.get("timestamp").cloned();
    match m["role"].as_str() {
        Some("user") => {
            let t = texts(&m["content"]);
            if t.is_empty() { vec![] } else { vec![line("user", session, ts, vec![text(&t)])] }
        }
        Some("assistant") => {
            let mut content = vec![];
            for item in m["content"].as_array().into_iter().flatten() {
                match item["type"].as_str() {
                    Some("text") => content.push(text(item["text"].as_str().unwrap_or(""))),
                    Some("toolCall") => content
                        .push(json!({"type": "tool_use", "id": item["id"], "name": item["name"],
                        "input": item["arguments"]})),
                    _ => {}
                }
            }
            if content.is_empty() { vec![] } else { vec![line("assistant", session, ts, content)] }
        }
        Some("toolResult") => vec![line(
            "user",
            session,
            ts,
            vec![json!({"type": "tool_result", "tool_use_id": m["toolCallId"], "content": cut(texts(&m["content"])),
                "is_error": m["isError"].as_bool().unwrap_or(false)})],
        )],
        _ => vec![],
    }
}

/// Gemini CLI's chat file: a metadata line, then `{"type":"user|gemini|info|…","content","toolCalls"?}` messages
/// and `{"$set":{…}}` patches.
pub fn from_gemini(record: &Value, session: &str) -> Vec<Value> {
    let ts = record.get("timestamp").cloned();
    match record["type"].as_str() {
        Some("user") => {
            let t = texts(&record["content"]);
            // The context Gemini CLI writes first isn't something anyone said.
            if t.is_empty() || t.starts_with("<session_context>") {
                vec![]
            } else {
                vec![line("user", session, ts, vec![text(&t)])]
            }
        }
        Some("gemini") => {
            let mut content = vec![];
            let t = texts(&record["content"]);
            if !t.is_empty() {
                content.push(text(&t));
            }
            let mut results = vec![];
            for call in record["toolCalls"].as_array().into_iter().flatten() {
                content
                    .push(json!({"type": "tool_use", "id": call["id"], "name": call["name"], "input": call["args"]}));
                if let Some(r) = call.get("result").filter(|r| !r.is_null()) {
                    let body = match r {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    results.push(json!({"type": "tool_result", "tool_use_id": call["id"], "content": cut(body),
                        "is_error": call["status"] == "error"}));
                }
            }
            let mut out = vec![];
            if !content.is_empty() {
                out.push(line("assistant", session, ts.clone(), content));
            }
            if !results.is_empty() {
                out.push(line("user", session, ts, results));
            }
            out
        }
        _ => vec![],
    }
}

/// Cursor's agent transcript: lines with a `role` and a `message.content` (the format isn't documented, so anything
/// else is passed over), or already in Claude's shape.
pub fn from_cursor(record: &Value, session: &str) -> Vec<Value> {
    let role =
        record["role"].as_str().or_else(|| record["type"].as_str()).or_else(|| record["message"]["role"].as_str());
    let kind = match role {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return vec![],
    };
    let raw = if record["message"].is_object() { &record["message"]["content"] } else { &record["content"] };
    let content: Vec<Value> = match raw {
        Value::String(s) if !s.is_empty() => vec![text(s)],
        Value::Array(items) => items
            .iter()
            .filter_map(|i| match i["type"].as_str() {
                Some("text") => Some(text(i["text"].as_str().unwrap_or(""))),
                Some("tool_use") | Some("tool_result") => Some(i.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    };
    if content.is_empty() {
        return vec![];
    }
    vec![line(kind, session, record.get("timestamp").cloned(), content)]
}

/// `opencode export <id>`: `{"info":{…},"messages":[{"info":{"role","time"},"parts":[…]}]}`.
pub fn from_opencode_export(export: &Value, session: &str) -> Vec<Value> {
    let mut out = vec![];
    for m in export["messages"].as_array().into_iter().flatten() {
        let ts = ms_to_iso(&m["info"]["time"]["created"]);
        let role = m["info"]["role"].as_str().unwrap_or("");
        let mut content = vec![];
        let mut results = vec![];
        for p in m["parts"].as_array().into_iter().flatten() {
            match p["type"].as_str() {
                Some("text") if p["synthetic"] != true => {
                    let t = p["text"].as_str().unwrap_or("");
                    if !t.is_empty() {
                        content.push(text(t));
                    }
                }
                Some("tool") => {
                    let state = &p["state"];
                    content.push(
                        json!({"type": "tool_use", "id": p["callID"], "name": p["tool"], "input": state["input"]}),
                    );
                    let body = state["output"].as_str().or_else(|| state["error"].as_str());
                    if let Some(body) = body {
                        results.push(json!({"type": "tool_result", "tool_use_id": p["callID"],
                            "content": cut(body.to_string()), "is_error": state["status"] == "error"}));
                    }
                }
                _ => {}
            }
        }
        let kind = if role == "user" { "user" } else { "assistant" };
        if !content.is_empty() {
            out.push(line(kind, session, ts.clone(), content));
        }
        if !results.is_empty() {
            out.push(line("user", session, ts, results));
        }
    }
    out
}

/// Convert a JSONL file line by line into `out`, replaced in one rename.
pub fn convert_jsonl(
    source: &Path,
    out: &Path,
    session: &str,
    each: fn(&Value, &str) -> Vec<Value>,
) -> Result<(), String> {
    let f = std::fs::File::open(source).map_err(|e| format!("reading {}: {e}", source.display()))?;
    let mut buf = Vec::new();
    for l in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
        for converted in each(&v, session) {
            serde_json::to_writer(&mut buf, &converted).map_err(|e| e.to_string())?;
            buf.write_all(b"\n").map_err(|e| e.to_string())?;
        }
    }
    crate::files::write_atomic(out, &buf)
}

pub fn write_lines(out: &Path, lines: &[Value]) -> Result<(), String> {
    let mut buf = Vec::new();
    for l in lines {
        serde_json::to_writer(&mut buf, l).map_err(|e| e.to_string())?;
        buf.push(b'\n');
    }
    crate::files::write_atomic(out, &buf)
}

/// Whether `out` needs writing again: missing, or older than `source`.
pub fn stale(source: &Path, out: &Path) -> bool {
    let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (m(source), m(out)) {
        (Some(s), Some(o)) => s > o,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn pi_messages_tool_calls_and_results() {
        let user = j(
            r#"{"type":"message","timestamp":"t1","message":{"role":"user","content":[{"type":"text","text":"RUN: echo hi"}]}}"#,
        );
        let call = j(
            r#"{"type":"message","timestamp":"t2","message":{"role":"assistant","content":[{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"echo hi"}}]}}"#,
        );
        let result = j(
            r#"{"type":"message","timestamp":"t3","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"hi\n"}],"isError":false}}"#,
        );
        let system = j(r#"{"type":"message","message":{"role":"system","content":"x"}}"#);
        assert_eq!(from_pi(&user, "s")[0]["message"]["content"][0]["text"], "RUN: echo hi");
        assert_eq!(from_pi(&user, "s")[0]["timestamp"], "t1");
        let c = &from_pi(&call, "s")[0];
        assert_eq!(
            (c["type"].as_str(), c["message"]["content"][0]["type"].as_str()),
            (Some("assistant"), Some("tool_use"))
        );
        assert_eq!(c["message"]["content"][0]["input"]["command"], "echo hi");
        let r = &from_pi(&result, "s")[0];
        assert_eq!((r["type"].as_str(), r["message"]["content"][0]["content"].as_str()), (Some("user"), Some("hi\n")));
        assert!(from_pi(&system, "s").is_empty());
        assert!(from_pi(&j(r#"{"type":"session","id":"x","cwd":"/a"}"#), "s").is_empty());
    }

    #[test]
    fn gemini_drops_its_own_context_and_patches() {
        let ctx = j(r#"{"type":"user","content":[{"text":"<session_context>\nThis is the Gemini CLI."}]}"#);
        let user = j(r#"{"type":"user","timestamp":"t","content":[{"text":"hello-gemini"}]}"#);
        let reply = j(
            r#"{"type":"gemini","content":"pong","toolCalls":[{"id":"t1","name":"run_shell_command","args":{"command":"ls"},"result":"a\nb","status":"success"}]}"#,
        );
        assert!(from_gemini(&ctx, "s").is_empty());
        assert!(from_gemini(&j(r#"{"$set":{"lastUpdated":"x"}}"#), "s").is_empty());
        assert!(from_gemini(&j(r#"{"sessionId":"s","projectHash":"h","kind":"main"}"#), "s").is_empty());
        assert_eq!(from_gemini(&user, "s")[0]["message"]["content"][0]["text"], "hello-gemini");
        let r = from_gemini(&reply, "s");
        assert_eq!(r.len(), 2, "the reply, then the tool's result");
        assert_eq!(r[0]["message"]["content"][1]["name"], "run_shell_command");
        assert_eq!(r[1]["message"]["content"][0]["content"], "a\nb");
    }

    #[test]
    fn opencode_exports_become_turns() {
        let e = j(r#"{"info":{"id":"ses_1"},"messages":[
          {"info":{"role":"user","time":{"created":1795856344819}},"parts":[{"type":"text","text":"first message"}]},
          {"info":{"role":"assistant","time":{"created":1795856344833}},"parts":[{"type":"step-start"},
            {"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"a"}},
            {"type":"text","text":"done"}]}]}"#);
        let out = from_opencode_export(&e, "ses_1");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["message"]["content"][0]["text"], "first message");
        assert!(out[0]["timestamp"].as_str().unwrap().starts_with("2026-11-28"));
        assert_eq!(out[1]["message"]["content"][0]["type"], "tool_use");
        assert_eq!(out[1]["message"]["content"][1]["text"], "done");
        assert_eq!(out[2]["message"]["content"][0]["tool_use_id"], "c1");
    }

    #[test]
    fn cursor_lines_with_a_role() {
        let u = j(r#"{"role":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#);
        let a = j(r#"{"role":"assistant","message":{"content":"hello"}}"#);
        assert_eq!(from_cursor(&u, "c")[0]["message"]["content"][0]["text"], "hi");
        assert_eq!(from_cursor(&a, "c")[0]["message"]["content"][0]["text"], "hello");
        assert!(from_cursor(&j(r#"{"role":"system","message":{"content":"x"}}"#), "c").is_empty());
    }

    #[test]
    fn long_results_are_cut() {
        assert!(cut("é".repeat(RESULT_MAX)).ends_with("…[cut]"));
        assert_eq!(cut("short".into()), "short");
    }
}
