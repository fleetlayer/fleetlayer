//! `fleetlayer mcp`'s bridge between an MCP client on stdio (Claude Code) and a tool on another node, one JSON-RPC line
//! at a time. Each request going up is timed: one that gets no response within the limit is answered here with a
//! JSON-RPC error ("didn't answer within 120s"), and a late response to it is dropped. So a machine that went to sleep or
//! a node restarting mid-call fails the call instead of leaving the client waiting (Claude Code waits a long time).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::Instant;

/// Copy lines both ways until the node's side ends. `label` names the tool in errors (`browser@laptop`).
pub async fn bridge<I, O, R, W>(stdin: I, mut stdout: O, from_node: R, mut to_node: W, label: &str, limit: Duration)
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut up = BufReader::new(stdin).lines();
    let mut down = BufReader::new(from_node).lines();
    let mut pending: HashMap<String, (Value, Instant)> = HashMap::new();
    let mut expired: HashSet<String> = HashSet::new();
    let mut stdin_open = true;
    loop {
        let next = pending.values().map(|(_, at)| *at).min();
        tokio::select! {
            // Both are cancel-safe: a line half read stays buffered.
            line = up.next_line(), if stdin_open => match line {
                Ok(Some(line)) => {
                    if let Some(id) = request_id(&line) {
                        pending.insert(id.to_string(), (id, Instant::now() + limit));
                    }
                    if to_node.write_all(format!("{line}\n").as_bytes()).await.is_err() || to_node.flush().await.is_err() {
                        break;
                    }
                }
                // The client is done: the tool gets end-of-file and exits, which ends `down`.
                _ => {
                    stdin_open = false;
                    let _ = to_node.shutdown().await;
                }
            },
            line = down.next_line() => match line {
                Ok(Some(line)) => {
                    if let Some(id) = response_id(&line) {
                        let key = id.to_string();
                        if expired.remove(&key) {
                            continue; // answered already, with the error
                        }
                        pending.remove(&key);
                    }
                    if stdout.write_all(format!("{line}\n").as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                        break;
                    }
                }
                _ => break,
            },
            _ = tokio::time::sleep_until(next.unwrap_or_else(Instant::now)), if next.is_some() => {
                let now = Instant::now();
                let late: Vec<String> = pending.iter().filter(|(_, (_, at))| *at <= now).map(|(k, _)| k.clone()).collect();
                for key in late {
                    let (id, _) = pending.remove(&key).expect("listed above");
                    let error = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": format!(
                        "{label} didn't answer within {}s (the machine may be asleep, or its node restarting)",
                        limit.as_secs()
                    )}});
                    expired.insert(key);
                    if stdout.write_all(format!("{error}\n").as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                        return;
                    }
                }
            }
        }
    }
    let _ = stdout.flush().await;
}

/// The id of a request (a line with `method` and `id`); notifications have no id and get no response.
fn request_id(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    (v.get("method").is_some() && v.get("id").is_some_and(|i| !i.is_null())).then(|| v["id"].clone())
}

/// The id of a response (a line with `id` and `result` or `error`, no `method`).
fn response_id(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    (v.get("method").is_none() && (v.get("result").is_some() || v.get("error").is_some())).then(|| v["id"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    type Lines = tokio::io::Lines<BufReader<tokio::io::DuplexStream>>;
    type Pipe = tokio::io::DuplexStream;

    /// The bridge with in-memory pipes: (client writes, client reads, tool reads, tool writes).
    fn start(limit: Duration) -> (Pipe, Lines, Lines, Pipe, tokio::task::JoinHandle<()>) {
        let (client_w, stdin) = duplex(1 << 16);
        let (stdout, client_r) = duplex(1 << 16);
        let (tool_w, from_node) = duplex(1 << 16);
        let (to_node, tool_r) = duplex(1 << 16);
        let task =
            tokio::spawn(async move { bridge(stdin, stdout, from_node, to_node, "browser@laptop", limit).await });
        (client_w, BufReader::new(client_r).lines(), BufReader::new(tool_r).lines(), tool_w, task)
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_that_never_answers_gets_an_error_and_later_calls_still_work() {
        let (mut client, mut client_out, mut tool_in, mut tool, _task) = start(Duration::from_secs(120));
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\"}\n").await.unwrap();
        assert!(tool_in.next_line().await.unwrap().unwrap().contains("\"id\":1"));
        // Nothing comes back: after 120 s the client gets an error for id 1.
        let line = client_out.next_line().await.unwrap().unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(
            v["error"]["message"],
            "browser@laptop didn't answer within 120s (the machine may be asleep, or its node restarting)"
        );
        // The late answer is dropped; the next call's answer goes through.
        tool.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n").await.unwrap();
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\"}\n").await.unwrap();
        assert!(tool_in.next_line().await.unwrap().unwrap().contains("\"id\":2"));
        tool.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n").await.unwrap();
        let v: Value = serde_json::from_str(&client_out.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["ok"], true);
    }

    #[tokio::test(start_paused = true)]
    async fn answers_in_time_and_notifications_pass_unchanged() {
        let (mut client, mut client_out, mut tool_in, mut tool, _task) = start(Duration::from_secs(120));
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n").await.unwrap();
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"a\",\"method\":\"tools/list\"}\n").await.unwrap();
        tool_in.next_line().await.unwrap();
        tool_in.next_line().await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
        tool.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n").await.unwrap();
        tool.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"a\",\"result\":{\"tools\":[]}}\n").await.unwrap();
        assert!(client_out.next_line().await.unwrap().unwrap().contains("progress"));
        assert!(client_out.next_line().await.unwrap().unwrap().contains("\"tools\""));
        // Long after, no error appears for the answered call: the tool closing ends the stream cleanly.
        tokio::time::sleep(Duration::from_secs(300)).await;
        drop(tool);
        assert!(client_out.next_line().await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_tool_side_ending_ends_the_bridge() {
        let (_client, mut client_out, _tool_in, tool, task) = start(Duration::from_secs(120));
        drop(tool);
        assert!(client_out.next_line().await.unwrap().is_none());
        task.await.unwrap();
    }
}
