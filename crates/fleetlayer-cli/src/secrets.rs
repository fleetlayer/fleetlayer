//! `fleetlayer secrets`: use the secrets delivered to this node's role, and keep them out of what leaves it.
//!
//! - `run -- <command>`: run a command with `op://…` (or any delivered reference) in its environment and env files
//!   replaced by the values, its output scrubbed. The replacement for `op run`, from what the role was granted.
//! - `scrub <path>…`: redact delivered secrets (and, with `--gitleaks`, whatever gitleaks finds) from files: the copy
//!   of a transcript before it's synced or backed up, never a live one.
//! - `list`: what's delivered to this node, by reference. `grants`: who may read what, fleet-wide.
//! - `forget`: remove everything delivered to this node.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use clap::Subcommand;
use fleetlayer_secrets::{Scrubber, Secret, registry};

#[derive(Subcommand)]
pub enum SecretsCommand {
    /// Run a command with the role's secrets: each environment variable, and each line of --env-file, whose value is
    /// a delivered reference (`op://vault/item/field`) gets the value instead. Its output is scrubbed of every
    /// delivered secret unless --no-masking.
    Run {
        /// A file of `NAME=value` lines (`#` comments), where a value can be a reference. More than one is fine.
        #[arg(long = "env-file")]
        env_files: Vec<PathBuf>,
        /// Pass the command's output through untouched (and keep its terminal), instead of redacting secrets from it
        /// line by line.
        #[arg(long)]
        no_masking: bool,
        /// The command and its arguments, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Redact every delivered secret from files, in place, as `[REDACTED <reference>]`. For the copy of a transcript
    /// that's about to be synced or backed up: live transcripts (`~/.claude/projects`, `~/.codex/sessions`) are
    /// refused.
    Scrub {
        /// Files or folders (every file below a folder).
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Also redact what gitleaks finds (secrets nobody delivered), as `[REDACTED gitleaks:<rule>]`. Needs gitleaks
        /// on PATH.
        #[arg(long)]
        gitleaks: bool,
        /// Say what would be redacted, change nothing, and exit 1 if anything would be.
        #[arg(long)]
        check: bool,
    },
    /// What's delivered to this node, by reference (never values), and which source brought it.
    List,
    /// Who may read what: every secrets source's grants (role, nodes, references, read or read-write), from the
    /// readings of every node this one may read.
    Grants,
    /// Remove everything delivered to this node: the files the plugins wrote and the registry. The next delivery puts
    /// them back.
    Forget,
    /// Forget which source a bundle is taken from (its node, or the key its source signs with), so the next sender is
    /// trusted from scratch: for a source that really moved or got a new key. Without a bundle, lists the pins.
    Unpin {
        /// The bundle (`claude-auth-manager/tokens`, `cookies/x`, `1password/<node>`).
        bundle: Option<String>,
    },
}

/// Every plugin's pinned sources on this machine: `~/.local/state/fleetlayer/<plugin>/sources.json`.
fn pin_files(home: &std::path::Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(home.join(".local/state/fleetlayer"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("sources.json"))
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

fn unpin(home: &std::path::Path, bundle: Option<&str>) -> anyhow::Result<i32> {
    let files = pin_files(home);
    let Some(bundle) = bundle else {
        for f in &files {
            for (b, source) in fleetlayer_secrets::pin::list(f) {
                println!("{b}  {source}  ({})", f.display());
            }
        }
        return Ok(0);
    };
    let mut done = false;
    for f in &files {
        if let Some(was) = fleetlayer_secrets::pin::unpin(f, bundle).map_err(anyhow::Error::msg)? {
            println!("{bundle} was taken only from {was}; the next sender is trusted from scratch ({}).", f.display());
            done = true;
        }
    }
    if !done {
        eprintln!("fleetlayer: no pin for {bundle} on this machine (`fleetlayer secrets unpin` lists them)");
        return Ok(1);
    }
    Ok(0)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Every delivered secret, by reference.
fn delivered() -> anyhow::Result<BTreeMap<String, String>> {
    let (secrets, errors) = registry::load(&home());
    for e in errors {
        eprintln!("fleetlayer: secrets registry: {e}");
    }
    Ok(secrets.into_iter().map(|s| (s.reference, s.value)).collect())
}

/// Whether `v` looks like a reference to a secret (so one that isn't delivered is an error, not passed through).
fn looks_like_reference(v: &str) -> bool {
    let v = v.trim();
    ["op://", "cookie://", "claude://"].iter().any(|p| v.starts_with(p)) && !v.contains(char::is_whitespace)
}

/// `NAME=value` lines: `#` comments and blank lines skipped, `export ` allowed, one layer of matching quotes removed.
fn parse_env_file(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = vec![];
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (k, v) = line.split_once('=').ok_or_else(|| format!("line {}: not NAME=value", i + 1))?;
        let k = k.trim();
        if !fleetlayer_secrets::write::env_var_ok(k) {
            return Err(format!("line {}: {k:?} isn't a variable name", i + 1));
        }
        let v = v.trim();
        let v = if v.len() >= 2 && (v.starts_with('"') && v.ends_with('"') || v.starts_with('\'') && v.ends_with('\''))
        {
            &v[1..v.len() - 1]
        } else {
            v
        };
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

/// The environment for the command: `base` with `files`' lines on top, every reference replaced. An error names the
/// references this node doesn't have.
fn resolve_env(
    base: Vec<(String, String)>,
    files: Vec<(String, String)>,
    values: &BTreeMap<String, String>,
) -> Result<Vec<(String, String)>, String> {
    let mut env: BTreeMap<String, String> = base.into_iter().collect();
    env.extend(files);
    let mut missing = vec![];
    for (k, v) in env.iter_mut() {
        if !looks_like_reference(v) {
            continue;
        }
        match values.get(v.trim()) {
            Some(value) => *v = value.clone(),
            None => missing.push(format!("{k}={}", v.trim())),
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "not delivered to this node: {}. A secret reaches a role only through a grant in its source's settings \
             (`fleetlayer secrets grants` shows them); ask the person to grant it to this role",
            missing.join(", ")
        ));
    }
    Ok(env.into_iter().collect())
}

pub async fn run(cmd: SecretsCommand) -> anyhow::Result<i32> {
    match cmd {
        SecretsCommand::Run { env_files, no_masking, command } => run_command(env_files, no_masking, command).await,
        SecretsCommand::Scrub { paths, gitleaks, check } => scrub(paths, gitleaks, check),
        SecretsCommand::List => {
            let (secrets, errors) = registry::load(&home());
            let dir = registry::dir(&home());
            let mut by_source: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().trim_end_matches(".json").to_string();
                let refs: Vec<String> = std::fs::read(e.path())
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .and_then(|v| v["secrets"].as_array().cloned())
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s["ref"].as_str().map(str::to_string))
                    .collect();
                by_source.insert(name, refs);
            }
            if secrets.is_empty() {
                println!("Nothing is delivered to this node ({}).", dir.display());
            }
            let short = Scrubber::new(&secrets).too_short().to_vec();
            for (source, refs) in by_source {
                println!("{source}:");
                for r in refs {
                    let note = if short.contains(&r) { "  (too short to redact)" } else { "" };
                    println!("  {r}{note}");
                }
            }
            for e in errors {
                eprintln!("fleetlayer: {e}");
            }
            Ok(0)
        }
        SecretsCommand::Unpin { bundle } => unpin(&home(), bundle.as_deref()),
        SecretsCommand::Grants | SecretsCommand::Forget => unreachable!("asks the node"),
    }
}

/// Print `secrets.grants` readings as a table: source node, role, nodes, access, reference.
pub fn print_grants(readings: &serde_json::Value) {
    let mut rows = vec![];
    for r in readings.as_array().into_iter().flatten() {
        let source = r["node"].as_str().unwrap_or("?");
        let d = &r["data"];
        let nodes: Vec<&str> = d["nodes"].as_array().into_iter().flatten().filter_map(|n| n.as_str()).collect();
        for g in d["items"].as_array().into_iter().flatten() {
            rows.push(format!(
                "{:<16} {:<14} {:<28} {:<10} {}",
                source,
                d["role"].as_str().unwrap_or("?"),
                nodes.join(","),
                g["access"].as_str().unwrap_or("read"),
                g["ref"].as_str().unwrap_or("?")
            ));
        }
    }
    if rows.is_empty() {
        println!("No secrets source publishes grants that this node may read.");
        return;
    }
    println!("{:<16} {:<14} {:<28} {:<10} reference", "source", "role", "nodes", "access");
    for r in rows {
        println!("{r}");
    }
}

async fn run_command(env_files: Vec<PathBuf>, no_masking: bool, command: Vec<String>) -> anyhow::Result<i32> {
    let values = delivered()?;
    let mut file_vars = vec![];
    for f in &env_files {
        let text = std::fs::read_to_string(f).map_err(|e| anyhow::anyhow!("reading {}: {e}", f.display()))?;
        file_vars.extend(parse_env_file(&text).map_err(|e| anyhow::anyhow!("{}: {e}", f.display()))?);
    }
    let base: Vec<(String, String)> = std::env::vars().collect();
    let env = resolve_env(base, file_vars, &values).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (program, args) = command.split_first().ok_or_else(|| anyhow::anyhow!("give a command after --"))?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).env_clear().envs(env);
    // Ctrl-C reaches the command (same process group); this process waits for it and passes its output on.
    tokio::spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });
    if no_masking {
        let status = cmd.status().await.map_err(|e| anyhow::anyhow!("running {program}: {e}"))?;
        return Ok(exit_code(status));
    }
    let secrets: Vec<Secret> = values.into_iter().map(|(reference, value)| Secret { reference, value }).collect();
    let scrubber = std::sync::Arc::new(Scrubber::new(&secrets));
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("running {program}: {e}"))?;
    let out = child.stdout.take().map(|s| tokio::spawn(mask(s, scrubber.clone(), false)));
    let err = child.stderr.take().map(|s| tokio::spawn(mask(s, scrubber.clone(), true)));
    let status = child.wait().await?;
    for t in [out, err].into_iter().flatten() {
        let _ = t.await;
    }
    Ok(exit_code(status))
}

fn exit_code(s: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(1))
}

/// Copy `r` to stdout or stderr line by line, redacted.
async fn mask(r: impl tokio::io::AsyncRead + Unpin, scrubber: std::sync::Arc<Scrubber>, stderr: bool) {
    use tokio::io::AsyncBufReadExt;
    let mut r = tokio::io::BufReader::new(r);
    let mut line = vec![];
    loop {
        line.clear();
        match r.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        let (out, _) = scrubber.scrub(&text);
        let bytes: &[u8] =
            if out == text && matches!(text, std::borrow::Cow::Borrowed(_)) { &line } else { out.as_bytes() };
        let res = if stderr {
            let mut e = std::io::stderr().lock();
            e.write_all(bytes).and_then(|_| e.flush())
        } else {
            let mut o = std::io::stdout().lock();
            o.write_all(bytes).and_then(|_| o.flush())
        };
        if res.is_err() {
            return;
        }
    }
}

/// Live transcript folders, which are refused: rewriting a file a running session appends to loses its lines.
fn live_folders(home: &Path) -> Vec<PathBuf> {
    vec![home.join(".claude/projects"), home.join(".codex/sessions")]
}

fn files_below(p: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let m = std::fs::symlink_metadata(p)?;
    if m.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(p)?.flatten().map(|e| e.path()).collect();
        entries.sort();
        for e in entries {
            files_below(&e, out)?;
        }
    } else if m.is_file() {
        out.push(p.to_path_buf());
    }
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Finding {
    #[serde(rename = "RuleID")]
    rule_id: String,
    secret: String,
    file: String,
}

/// gitleaks' findings below `path`, by file.
fn gitleaks(path: &Path) -> anyhow::Result<BTreeMap<PathBuf, Vec<Secret>>> {
    let out = std::process::Command::new("gitleaks")
        .args(["dir", "--no-banner", "--report-format", "json", "--report-path", "-", "--exit-code", "0"])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("running gitleaks (is it on PATH?): {e}"))?;
    if !out.status.success() {
        anyhow::bail!("gitleaks failed on {} ({})", path.display(), out.status);
    }
    let findings: Vec<Finding> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let mut by_file: BTreeMap<PathBuf, Vec<Secret>> = BTreeMap::new();
    for f in findings {
        if f.secret.trim().len() >= fleetlayer_secrets::scrub::MIN_LEN {
            let file = PathBuf::from(&f.file);
            let file = if file.is_absolute() { file } else { path.join(file) };
            by_file
                .entry(file)
                .or_default()
                .push(Secret { reference: format!("gitleaks:{}", f.rule_id), value: f.secret });
        }
    }
    Ok(by_file)
}

fn scrub(paths: Vec<PathBuf>, use_gitleaks: bool, check: bool) -> anyhow::Result<i32> {
    let home = home();
    let live = live_folders(&home);
    let mut files = vec![];
    let mut found_by_gitleaks: BTreeMap<PathBuf, Vec<Secret>> = BTreeMap::new();
    for p in &paths {
        let real = std::fs::canonicalize(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
        if let Some(l) = live.iter().find(|l| std::fs::canonicalize(l).is_ok_and(|l| real.starts_with(&l))) {
            anyhow::bail!(
                "{} is inside {}, where sessions write their transcripts as they run; scrub the copy that leaves \
                 (after it's copied for the sync or backup), never the live files",
                p.display(),
                l.display()
            );
        }
        files_below(&real, &mut files).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
        if use_gitleaks {
            for (f, s) in gitleaks(&real)? {
                let f = std::fs::canonicalize(&f).unwrap_or(f);
                found_by_gitleaks.entry(f).or_default().extend(s);
            }
        }
    }
    let delivered: Vec<Secret> =
        delivered()?.into_iter().map(|(reference, value)| Secret { reference, value }).collect();
    if delivered.is_empty() && !use_gitleaks {
        eprintln!("fleetlayer: nothing is delivered to this node, so there's nothing to redact (add --gitleaks?)");
    }
    let base = Scrubber::new(&delivered);
    let mut changed = 0;
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue }; // binary: not text a transcript holds
        let extra = found_by_gitleaks.get(&f);
        let local;
        let scrubber = match extra {
            Some(x) => {
                let mut all = delivered.clone();
                all.extend(x.iter().cloned());
                local = Scrubber::new(&all);
                &local
            }
            None => &base,
        };
        let (out, hits) = scrubber.scrub(&text);
        if hits.is_empty() {
            continue;
        }
        changed += 1;
        let what: Vec<String> = hits.iter().map(|h| format!("{} ({})", h.reference, h.form.label())).collect();
        println!("{}: {}", f.display(), what.join(", "));
        if !check {
            replace_keeping_mode(&f, &out)?;
        }
    }
    if changed > 0 && check {
        return Ok(1);
    }
    if changed > 0 {
        eprintln!(
            "fleetlayer: redacted secrets in {changed} file(s). They were in a transcript, so they reached the model's \
             API: rotate each one named above."
        );
    }
    Ok(0)
}

/// Replace `path` with `content` in one rename, keeping its permission bits.
fn replace_keeping_mode(path: &Path, content: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    fleetlayer_secrets::write::atomic(path, content)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_files_parse_and_references_resolve() {
        let lines = parse_env_file("# c\nexport GITHUB_TOKEN=op://github/bot/token\nPLAIN='x y'\n\nQ=\"op://a/b/c\"\n")
            .unwrap();
        assert_eq!(lines[0], ("GITHUB_TOKEN".into(), "op://github/bot/token".into()));
        assert_eq!(lines[1], ("PLAIN".into(), "x y".into()));
        let values: BTreeMap<String, String> =
            [("op://github/bot/token".to_string(), "ghp_x".to_string()), ("op://a/b/c".into(), "v".into())].into();
        let env = resolve_env(vec![("HOME".into(), "/h".into())], lines, &values).unwrap();
        let env: BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(env["GITHUB_TOKEN"], "ghp_x");
        assert_eq!(env["Q"], "v");
        assert_eq!(env["HOME"], "/h");
        let e = resolve_env(vec![("X".into(), "op://nope/i/f".into())], vec![], &values).unwrap_err();
        assert!(e.contains("X=op://nope/i/f") && e.contains("grant"), "{e}");
        assert!(parse_env_file("1X=y").is_err());
    }

    #[test]
    fn live_transcripts_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let live = d.path().join(".claude/projects/p");
        std::fs::create_dir_all(&live).unwrap();
        assert!(live_folders(d.path()).iter().any(|l| live.starts_with(l)));
    }

    #[test]
    fn unpin_finds_the_bundle_in_any_plugins_pins() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join(".local/state/fleetlayer/claude-auth-manager/sources.json");
        fleetlayer_secrets::pin::check(&file, "claude-auth-manager/tokens", Some("laptop"), None, None, 1).unwrap();
        assert_eq!(pin_files(home.path()), std::slice::from_ref(&file));
        assert_eq!(unpin(home.path(), Some("cookies/x")).unwrap(), 1, "not pinned anywhere");
        assert_eq!(unpin(home.path(), Some("claude-auth-manager/tokens")).unwrap(), 0);
        assert!(fleetlayer_secrets::pin::list(&file).is_empty());
        assert_eq!(unpin(home.path(), None).unwrap(), 0, "listing");
    }
}
