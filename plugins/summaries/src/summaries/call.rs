//! One headless Claude Code call: a system prompt, a JSON schema and a prompt in, the structured answer out. Ported
//! from claude-summarizer's `summarise.call`, with the same lean flags.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use super::accounts::Account;

#[derive(Debug, Clone)]
pub struct CallRequest {
    pub system_prompt: String,
    pub schema: String,
    pub prompt: String,
    pub account: Account,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallStats {
    pub seconds: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub account: String,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum CallError {
    #[error("claude -p timed out after {0}s")]
    Timeout(u64),
    /// A usage limit: stop calling until an account has room.
    #[error("usage limit: {0}")]
    Limit(String),
    #[error("{0}")]
    Failed(String),
}

pub type CallFuture = Pin<Box<dyn Future<Output = Result<(Value, CallStats), CallError>> + Send>>;

/// The model behind summaries and catch-ups. `ClaudeCli` in the node; a fake in tests.
pub trait Model: Send + Sync {
    fn call(&self, req: CallRequest) -> CallFuture;
}

#[derive(Debug, Clone)]
pub struct ClaudeCli {
    /// The `claude` binary.
    pub program: PathBuf,
    pub model: String,
    /// The run's cwd: an empty directory, so no project and no CLAUDE.md.
    pub work_dir: PathBuf,
    pub timeout: Duration,
}

impl ClaudeCli {
    pub fn args(&self, req: &CallRequest) -> Vec<String> {
        [
            "-p",
            "--model",
            &self.model,
            "--tools",
            "",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--disable-slash-commands",
            "--setting-sources",
            "",
            "--output-format",
            "json",
            "--system-prompt",
            &req.system_prompt,
            "--json-schema",
            &req.schema,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }
}

impl Model for ClaudeCli {
    fn call(&self, req: CallRequest) -> CallFuture {
        let me = self.clone();
        Box::pin(async move {
            tokio::fs::create_dir_all(&me.work_dir).await.map_err(|e| CallError::Failed(format!("work dir: {e}")))?;
            let mut cmd = tokio::process::Command::new(&me.program);
            cmd.args(me.args(&req))
                .env_remove("CLAUDE_CONFIG_DIR")
                .envs(req.account.env())
                .current_dir(&me.work_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let started = Instant::now();
            let mut child =
                cmd.spawn().map_err(|e| CallError::Failed(format!("can't run {}: {e}", me.program.display())))?;
            let mut stdin = child.stdin.take().expect("piped");
            let prompt = req.prompt.clone();
            let writer = tokio::spawn(async move {
                let _ = stdin.write_all(prompt.as_bytes()).await;
            });
            let out = match tokio::time::timeout(me.timeout, child.wait_with_output()).await {
                Ok(out) => out.map_err(|e| CallError::Failed(format!("claude -p: {e}")))?,
                Err(_) => return Err(CallError::Timeout(me.timeout.as_secs())),
            };
            let _ = writer.await;
            parse_output(&out.stdout, &out.stderr, out.status.success(), started.elapsed(), &req.account)
        })
    }
}

fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Turn `claude -p --output-format json` output into the structured answer and the call's stats.
pub fn parse_output(
    stdout: &[u8],
    stderr: &[u8],
    success: bool,
    elapsed: Duration,
    account: &Account,
) -> Result<(Value, CallStats), CallError> {
    let out: Value = serde_json::from_slice(stdout).map_err(|_| {
        let text = if stderr.is_empty() { stdout } else { stderr };
        let msg = format!(
            "claude -p exited {}: {}",
            if success { "0" } else { "non-zero" },
            cut(String::from_utf8_lossy(text).trim(), 400)
        );
        classify(msg)
    })?;
    let usage = out.get("usage").cloned().unwrap_or(Value::Null);
    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let stats = CallStats {
        seconds: (elapsed.as_secs_f64() * 10.0).round() / 10.0,
        input_tokens: n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"),
        output_tokens: n("output_tokens"),
        cost_usd: out.get("total_cost_usd").and_then(Value::as_f64),
        account: account.id.clone(),
    };
    let result_text = match out.get("result") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    if out.get("is_error").and_then(Value::as_bool).unwrap_or(false) || !success {
        let subtype = out.get("subtype").and_then(Value::as_str).unwrap_or("None");
        return Err(classify(format!("claude -p failed ({subtype}): {}", cut(&result_text, 400))));
    }
    match out.get("structured_output") {
        Some(v @ Value::Object(_)) => Ok((v.clone(), stats)),
        _ => match serde_json::from_str::<Value>(&result_text) {
            Ok(v @ Value::Object(_)) => Ok((v, stats)),
            _ => Err(CallError::Failed(format!("no structured output: {}", cut(&result_text, 300)))),
        },
    }
}

/// claude-summarizer stopped the run on any error mentioning "limit".
fn classify(msg: String) -> CallError {
    if msg.to_lowercase().contains("limit") { CallError::Limit(msg) } else { CallError::Failed(msg) }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Summary {
    Update { page: String, paragraph: String, sentence: String },
    NoChange,
}

/// The summary schema's answer, checked as claude-summarizer did: an update needs all three tiers.
pub fn parse_summary(v: &Value) -> Result<Summary, CallError> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    match s("status").as_str() {
        "no_change" => Ok(Summary::NoChange),
        "update" => {
            let (page, paragraph, sentence) = (s("page"), s("paragraph"), s("sentence"));
            if page.is_empty() || paragraph.is_empty() || sentence.is_empty() {
                return Err(CallError::Failed("update without all three tiers".into()));
            }
            Ok(Summary::Update { page, paragraph, sentence })
        }
        other => Err(CallError::Failed(format!("unknown status '{other}'"))),
    }
}

pub fn parse_catchup(v: &Value) -> Result<String, CallError> {
    let text = v.get("catchup").and_then(Value::as_str).unwrap_or("").trim().to_owned();
    if text.is_empty() { Err(CallError::Failed("empty catch-up".into())) } else { Ok(text) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn acct() -> Account {
        Account::default_login()
    }

    #[test]
    fn parses_structured_output_and_stats() {
        let out = json!({"type": "result", "subtype": "success", "is_error": false, "result": "",
            "structured_output": {"status": "update", "page": " P ", "paragraph": "Q", "sentence": "S"},
            "usage": {"input_tokens": 3, "cache_creation_input_tokens": 1000, "cache_read_input_tokens": 20, "output_tokens": 400},
            "total_cost_usd": 0.03});
        let (v, stats) =
            parse_output(out.to_string().as_bytes(), b"", true, Duration::from_millis(12_345), &acct()).unwrap();
        assert_eq!(
            stats,
            CallStats {
                seconds: 12.3,
                input_tokens: 1023,
                output_tokens: 400,
                cost_usd: Some(0.03),
                account: "default".into()
            }
        );
        assert_eq!(
            parse_summary(&v).unwrap(),
            Summary::Update { page: "P".into(), paragraph: "Q".into(), sentence: "S".into() }
        );
    }

    #[test]
    fn falls_back_to_the_result_text_and_checks_tiers() {
        let out = json!({"is_error": false, "result": "{\"status\": \"no_change\"}"});
        let (v, _) = parse_output(out.to_string().as_bytes(), b"", true, Duration::ZERO, &acct()).unwrap();
        assert_eq!(parse_summary(&v).unwrap(), Summary::NoChange);
        assert!(parse_summary(&json!({"status": "update", "page": "p"})).is_err());
        assert!(parse_summary(&json!({"status": "maybe"})).is_err());
    }

    #[test]
    fn errors_and_limits() {
        let out = json!({"is_error": true, "subtype": "error_during_execution", "result": "Claude AI usage limit reached|1795400000"});
        assert!(matches!(
            parse_output(out.to_string().as_bytes(), b"", false, Duration::ZERO, &acct()),
            Err(CallError::Limit(_))
        ));
        assert!(
            matches!(parse_output(b"not json", b"boom", false, Duration::ZERO, &acct()), Err(CallError::Failed(m)) if m.contains("boom"))
        );
    }

    #[test]
    fn lean_flags() {
        let cli = ClaudeCli {
            program: "claude".into(),
            model: "sonnet".into(),
            work_dir: "/tmp/w".into(),
            timeout: Duration::from_secs(420),
        };
        let req = CallRequest { system_prompt: "SYS".into(), schema: "{}".into(), prompt: "P".into(), account: acct() };
        assert_eq!(
            cli.args(&req),
            [
                "-p",
                "--model",
                "sonnet",
                "--tools",
                "",
                "--strict-mcp-config",
                "--no-session-persistence",
                "--disable-slash-commands",
                "--setting-sources",
                "",
                "--output-format",
                "json",
                "--system-prompt",
                "SYS",
                "--json-schema",
                "{}",
            ]
        );
    }

    #[tokio::test]
    async fn runs_a_fake_claude_with_the_account_env() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("claude");
        std::fs::write(&script, "#!/bin/sh\nprompt=$(cat)\nprintf '{\"is_error\":false,\"result\":\"\",\"structured_output\":{\"catchup\":\"%s %s %s\"}}' \"$prompt\" \"$CLAUDE_ACCOUNT_ROTATOR_REFRESH\" \"${CLAUDE_CONFIG_DIR:-none}\"\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let cli = ClaudeCli {
            program: script,
            model: "sonnet".into(),
            work_dir: dir.path().join("work"),
            timeout: Duration::from_secs(20),
        };
        let req =
            CallRequest { system_prompt: "S".into(), schema: "{}".into(), prompt: "hello".into(), account: acct() };
        let (v, _) = cli.call(req).await.unwrap();
        assert_eq!(parse_catchup(&v).unwrap(), "hello 1 none");
    }
}
