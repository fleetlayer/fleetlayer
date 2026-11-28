//! Shared commands: a node runs a few commands it declares, with typed parameters, for sessions on other nodes
//! (`docs/guides/shared-commands.md`). The person's laptop can let agents in VMs run `rack-gateway` there (its own
//! session, its own key, its own dialog) without giving them a shell:
//!
//! ```yaml
//! commands:
//!   - name: rack-gateway-run
//!     description: Run a script on a rack (one key touch per run)
//!     run: [/opt/homebrew/bin/rack-gateway, run, --rack, "{rack}", --script, "{script}", --justification, "{why}"]
//!     params:
//!       rack: { type: choice, values: [staging, production] }
//!       script: { type: file, max_bytes: 65536, extension: .rb }
//!       why: { type: text, max_length: 200 }
//!     who: ["*@agents"]
//!     timeout: 30m
//! ```
//!
//! A caller runs one with `fleetlayer run rack-gateway-run@laptop --rack staging --script fix.rb --why "…"`. Its node
//! asks the control plane (`link/command_open`), which refuses it outright if the policy denies `run` on
//! `command:<name>@<node>` and otherwise asks this node to take it (`link/command_serve`); both join a stream. The
//! caller sends one request line (its arguments, file parameters with their bytes and SHA-256); this node checks every
//! argument against its type, then asks the control plane about this exact run (`link/command_call`), which decides,
//! audits it with the program and arguments, and holds it for the person when an `ask:` rule matches. Only then are the
//! files written (a fresh private folder this node chooses) and the program started: by direct exec, never a shell.
//! Its output streams back line by line, scrubbed of delivered secrets; the caller can cancel; the timeout kills it.
//!
//! The stream carries one JSON object per line. Caller → node: `{"type":"run","args":{…},"files":{"<param>":
//! {"name","sha256","base64"}}}`, then optionally `{"type":"cancel"}`. Node → caller: `waiting` (held for the
//! person: `approval`, `reason`), `started`, `output` (`stream`: stdout or stderr, `text`), `note`, then one of
//! `refused` (`reason`) or `exit` (`code`, or `signal` with `reason`: timeout, cancelled).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use fleetlayer_protocol::link::{self as l, CommandInfo, CommandOpen, CommandServe, ParamInfo, ToolCall, ToolOpened};
use fleetlayer_protocol::rpc::RpcError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::Node;

/// The longest a command may be given to run, and the default.
const TIMEOUT_MAX: u64 = 24 * 3600;
const TIMEOUT_DEFAULT: u64 = 3600;
/// The largest file parameter, and the default.
const FILE_MAX: u64 = 16 << 20;
const FILE_DEFAULT: u64 = 1 << 20;
/// The longest text, and the default.
const TEXT_MAX: usize = 4096;
const TEXT_DEFAULT: usize = 200;
/// The most output a run sends back, and the default; past it, output is dropped with a note (the program runs on).
const OUTPUT_MAX: u64 = 64 << 20;
const OUTPUT_DEFAULT: u64 = 4 << 20;
/// How long a run waits for the person's approval, by default and at most.
const APPROVAL_WAIT_DEFAULT: u64 = 600;
const APPROVAL_WAIT_MAX: u64 = 3600;
/// How often to look again while it waits.
const POLL: Duration = Duration::from_secs(5);
/// A line of output longer than this goes out in pieces.
const LINE_MAX: usize = 16 * 1024;
/// How long the caller has to send its request once the stream opens.
const REQUEST_WAIT: Duration = Duration::from_secs(60);
/// How long a program has to stop after SIGTERM before it's killed.
const GRACE: Duration = Duration::from_secs(5);
/// The environment every command gets from the node's own (plus its `env:` names). Nothing else: the node's
/// environment can hold tokens.
const BASE_ENV: &[&str] = &["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ"];
/// Parameter names the CLI takes for itself.
const RESERVED: &[&str] = &["why", "help"];

/// One command in node.yaml's `commands:`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandConfig {
    /// 1 to 64 letters, digits, `-` and `_`.
    pub name: String,
    /// One line for agents choosing a command (`fleetlayer commands` shows it).
    #[serde(default)]
    pub description: Option<String>,
    /// The program and its arguments, one list item per argument, with `{param}` where a parameter's value goes
    /// (`{{` and `}}` for literal braces). The program itself is fixed: no parameter in the first item.
    pub run: Vec<String>,
    #[serde(default)]
    pub params: BTreeMap<String, ParamConfig>,
    /// `<tab>@<node>` patterns (`*` as a wildcard) that may run it. Nobody when empty; the access policy decides too.
    #[serde(default)]
    pub who: Vec<String>,
    /// The longest a run may take (`90s`, `30m`, `2h`, or seconds), then it's stopped. Default 1h, at most 24h.
    #[serde(default)]
    pub timeout: Option<Duration_>,
    /// Names of this node's environment variables the program also gets (besides PATH, HOME, USER, the locale and a
    /// few more). Values never come from the caller.
    #[serde(default)]
    pub env: Vec<String>,
    /// The directory it runs in. Default: the run's own private folder, removed after.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// The most output sent back, in bytes (default 4 MiB, at most 64 MiB).
    #[serde(default)]
    pub max_output: Option<u64>,
    /// How long a run waits for the person when the access policy asks them (`10m` by default, at most `1h`).
    #[serde(default)]
    pub approval_wait: Option<Duration_>,
}

/// A duration in node.yaml: seconds, or a number with `s`, `m`, `h` or `d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Duration_(pub u64);

impl<'de> Deserialize<'de> for Duration_ {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Secs(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Secs(s) => Ok(Duration_(s)),
            Raw::Text(t) => parse_duration(&t).map(Duration_).map_err(serde::de::Error::custom),
        }
    }
}

fn parse_duration(t: &str) -> Result<u64, String> {
    let t = t.trim();
    let (n, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len()));
    let n: u64 = n.parse().map_err(|_| format!("{t:?} isn't a duration (90s, 30m, 2h)"))?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("{t:?} isn't a duration (90s, 30m, 2h)")),
    };
    Ok(n.saturating_mul(mult))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParamConfig {
    /// `choice`, `text`, `pattern` or `file`.
    #[serde(rename = "type")]
    pub kind: ParamKind,
    #[serde(default)]
    pub description: Option<String>,
    /// choice: the values it takes.
    #[serde(default)]
    pub values: Vec<String>,
    /// pattern: a regular expression the whole value must match.
    #[serde(default)]
    pub pattern: Option<String>,
    /// text and pattern: the longest value, in characters (default 200, at most 4096).
    #[serde(default)]
    pub max_length: Option<usize>,
    /// file: the largest, in bytes (default 1 MiB, at most 16 MiB).
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// file: the extension the caller's file name must end with (`.sh`); the written file gets it too.
    #[serde(default)]
    pub extension: Option<String>,
    /// The value when the caller gives none (not for files). Without one, the parameter must be given.
    #[serde(default)]
    pub default: Option<String>,
    /// text: allow a value starting with `-`. Off by default: a program can read such a value as an option of its
    /// own (`--rack=production` in place of a reason).
    #[serde(default)]
    pub allow_dash: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    Choice,
    Text,
    Pattern,
    File,
}

impl ParamKind {
    fn name(self) -> &'static str {
        match self {
            ParamKind::Choice => "choice",
            ParamKind::Text => "text",
            ParamKind::Pattern => "pattern",
            ParamKind::File => "file",
        }
    }
}

/// One piece of an argument template.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Lit(String),
    Param(String),
}

/// `--app={app}` → `[Lit("--app="), Param("app")]`; `{{` and `}}` are literal braces.
fn pieces(arg: &str) -> Result<Vec<Piece>, String> {
    let mut out = vec![];
    let mut lit = String::new();
    let mut chars = arg.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                lit.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                lit.push('}');
            }
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => name.push(c),
                        None => return Err(format!("{arg:?}: a {{ without its }} (write {{{{ for a brace)")),
                    }
                }
                if !valid_param_name(&name) {
                    return Err(format!("{arg:?}: {{{name}}} isn't a parameter name"));
                }
                if !lit.is_empty() {
                    out.push(Piece::Lit(std::mem::take(&mut lit)));
                }
                out.push(Piece::Param(name));
            }
            '}' => return Err(format!("{arg:?}: a }} without its {{ (write }}}} for a brace)")),
            c => lit.push(c),
        }
    }
    if !lit.is_empty() || out.is_empty() {
        out.push(Piece::Lit(lit));
    }
    Ok(out)
}

fn valid_param_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 40
        && n.starts_with(|c: char| c.is_ascii_lowercase())
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn valid_extension(e: &str) -> bool {
    e.len() >= 2 && e.len() <= 17 && e.starts_with('.') && e[1..].chars().all(|c| c.is_ascii_alphanumeric())
}

/// Text a person reads as it is: no control characters and nothing that hides itself.
fn plain(v: &str) -> bool {
    !v.chars().any(char::is_control) && crate::gate::visible(v) == v
}

impl CommandConfig {
    /// Whether the settings make sense together.
    pub fn check(&self) -> Result<(), String> {
        if !fleetlayer_valid_name(&self.name) {
            return Err("a name is 1 to 64 letters, digits, - and _, starting with a letter or digit".into());
        }
        let Some(program) = self.run.first() else { return Err("run: the program and its arguments".into()) };
        if program.is_empty() || pieces(program)?.iter().any(|p| matches!(p, Piece::Param(_))) {
            return Err("run: the program (the first item) is fixed: no {parameter} in it".into());
        }
        let mut used = std::collections::BTreeSet::new();
        for a in &self.run {
            for p in pieces(a)? {
                if let Piece::Param(n) = p {
                    if !self.params.contains_key(&n) {
                        return Err(format!("run: {{{n}}} isn't one of its params"));
                    }
                    used.insert(n);
                }
            }
        }
        for (name, p) in &self.params {
            let at = |e: &str| format!("params.{name}: {e}");
            if !valid_param_name(name) {
                return Err(at("a name is lower-case letters, digits, _ and -, starting with a letter"));
            }
            if RESERVED.contains(&name.as_str()) {
                return Err(at("the fleetlayer command line takes --why and --help itself: call it something else"));
            }
            if !used.contains(name) {
                return Err(at("not used in run:"));
            }
            if p.kind != ParamKind::Choice && !p.values.is_empty() {
                return Err(at("values: are for a choice"));
            }
            if p.kind != ParamKind::Pattern && p.pattern.is_some() {
                return Err(at("pattern: is for a pattern"));
            }
            if p.kind != ParamKind::File && (p.max_bytes.is_some() || p.extension.is_some()) {
                return Err(at("max_bytes and extension are for a file"));
            }
            if p.allow_dash && p.kind != ParamKind::Text {
                return Err(at("allow_dash is for text (a choice or pattern decides its own values)"));
            }
            if p.max_length.is_some_and(|m| m == 0 || m > TEXT_MAX) {
                return Err(at(&format!("max_length is 1 to {TEXT_MAX}")));
            }
            match p.kind {
                ParamKind::Choice => {
                    if p.values.is_empty() {
                        return Err(at("a choice lists its values:"));
                    }
                    if p.values.iter().any(|v| v.is_empty() || !plain(v)) {
                        return Err(at("a value is plain text, not empty"));
                    }
                }
                ParamKind::Pattern => {
                    let Some(re) = &p.pattern else { return Err(at("a pattern gives its pattern:")) };
                    regex::Regex::new(&format!("^(?:{re})$")).map_err(|e| at(&format!("pattern: {e}")))?;
                }
                ParamKind::File => {
                    if p.default.is_some() {
                        return Err(at("a file has no default"));
                    }
                    if p.max_bytes.is_some_and(|m| m == 0 || m > FILE_MAX) {
                        return Err(at(&format!("max_bytes is 1 to {FILE_MAX}")));
                    }
                    if p.extension.as_deref().is_some_and(|e| !valid_extension(e)) {
                        return Err(at("extension is a dot and 1 to 16 letters or digits (.sh)"));
                    }
                }
                ParamKind::Text => {}
            }
            if let Some(d) = &p.default {
                check_value(name, p, d).map_err(|e| at(&format!("default: {e}")))?;
            }
        }
        if self.timeout.is_some_and(|t| t.0 == 0 || t.0 > TIMEOUT_MAX) {
            return Err("timeout is 1s to 24h".into());
        }
        if self.approval_wait.is_some_and(|t| t.0 > APPROVAL_WAIT_MAX) {
            return Err("approval_wait is at most 1h".into());
        }
        if self.max_output.is_some_and(|m| m == 0 || m > OUTPUT_MAX) {
            return Err(format!("max_output is 1 to {OUTPUT_MAX} bytes"));
        }
        for e in &self.env {
            if e.is_empty()
                || !e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || e.starts_with(|c: char| c.is_ascii_digit())
            {
                return Err(format!("env: {e:?} isn't a variable name"));
            }
        }
        Ok(())
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout.map_or(TIMEOUT_DEFAULT, |t| t.0))
    }

    fn approval_wait(&self) -> Duration {
        Duration::from_secs(self.approval_wait.map_or(APPROVAL_WAIT_DEFAULT, |t| t.0))
    }

    /// Its parameters in the order they first appear in `run:`.
    fn ordered(&self) -> Vec<(&String, &ParamConfig)> {
        let mut out: Vec<(&String, &ParamConfig)> = vec![];
        for a in &self.run {
            for p in pieces(a).unwrap_or_default() {
                if let Piece::Param(n) = p
                    && let Some((k, v)) = self.params.get_key_value(&n)
                    && !out.iter().any(|(o, _)| *o == k)
                {
                    out.push((k, v));
                }
            }
        }
        out
    }

    /// What `link/tools` says about it: never the program or its arguments.
    pub fn info(&self) -> CommandInfo {
        CommandInfo {
            name: self.name.clone(),
            description: self.description.clone(),
            params: self
                .ordered()
                .into_iter()
                .map(|(name, p)| ParamInfo {
                    name: name.clone(),
                    kind: p.kind.name().into(),
                    description: p.description.clone(),
                    values: p.values.clone(),
                    pattern: p.pattern.clone(),
                    max_length: match p.kind {
                        ParamKind::Text | ParamKind::Pattern => Some(p.max_length.unwrap_or(TEXT_DEFAULT)),
                        _ => None,
                    },
                    max_bytes: (p.kind == ParamKind::File).then(|| p.max_bytes.unwrap_or(FILE_DEFAULT)),
                    extension: p.extension.clone(),
                    default: p.default.clone(),
                })
                .collect(),
            who: self.who.clone(),
            timeout: self.timeout().as_secs(),
        }
    }
}

/// The control plane's rule for a command's name.
fn fleetlayer_valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && n.starts_with(|c: char| c.is_ascii_alphanumeric())
}

/// A value for a parameter that isn't a file: None when it fits its type.
fn check_value(name: &str, p: &ParamConfig, v: &str) -> Result<(), String> {
    match p.kind {
        ParamKind::Choice => {
            if !p.values.iter().any(|x| x == v) {
                return Err(format!("--{name} is one of {}", p.values.join(", ")));
            }
        }
        ParamKind::Text | ParamKind::Pattern => {
            let max = p.max_length.unwrap_or(TEXT_DEFAULT);
            if v.chars().count() > max {
                return Err(format!("--{name} is at most {max} characters"));
            }
            if !plain(v) {
                return Err(format!("--{name} has control characters or characters that hide themselves"));
            }
            if p.kind == ParamKind::Text && !p.allow_dash && v.starts_with('-') {
                return Err(format!("--{name} can't start with - (the program could read it as an option)"));
            }
            if let Some(re) = &p.pattern {
                let re = regex::Regex::new(&format!("^(?:{re})$")).map_err(|e| e.to_string())?;
                if !re.is_match(v) {
                    return Err(format!("--{name} must match {}", p.pattern.as_deref().unwrap_or_default()));
                }
            }
        }
        ParamKind::File => return Err(format!("--{name} is a file")),
    }
    Ok(())
}

/// A file parameter as the caller sends it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileArg {
    /// The caller's file name (no folders): checked against the extension and shown to the person.
    pub name: String,
    pub sha256: String,
    pub base64: String,
}

/// The caller's request line.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    #[serde(default)]
    pub files: BTreeMap<String, FileArg>,
}

/// A file to write before the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub extension: String,
}

impl File {
    fn file_name(&self) -> String {
        format!("{}{}", self.sha256, self.extension)
    }
}

/// A request checked against the command: what will run, what the person and the audit log see, and the hash an
/// approval `for: once` is bound to.
#[derive(Debug)]
pub struct Checked {
    /// Each argument as pieces: literal text, or a file to substitute by its path once written.
    argv: Vec<Vec<Arg>>,
    pub files: Vec<File>,
    pub detail: String,
    pub args_hash: String,
}

#[derive(Debug, Clone)]
enum Arg {
    Text(String),
    File(usize),
}

impl Checked {
    /// The program and its arguments with each file's path in `dir`.
    pub fn argv(&self, dir: &Path) -> Vec<String> {
        self.argv
            .iter()
            .map(|parts| {
                parts
                    .iter()
                    .map(|p| match p {
                        Arg::Text(t) => t.clone(),
                        Arg::File(i) => dir.join(self.files[*i].file_name()).to_string_lossy().into_owned(),
                    })
                    .collect::<String>()
            })
            .collect()
    }
}

/// Check a caller's arguments against the command, every one against its type. Anything undeclared, missing, or not
/// matching is refused, with the first problem.
pub fn check(cmd: &CommandConfig, req: &RunRequest, caller: &str, node: &str) -> Result<Checked, String> {
    if req.kind != "run" {
        return Err(format!("expected a run request, got {:?}", req.kind));
    }
    for k in req.args.keys().chain(req.files.keys()) {
        if !cmd.params.contains_key(k) {
            return Err(format!("{} takes no --{k} ({})", cmd.name, usage(cmd)));
        }
    }
    let mut values: BTreeMap<&str, String> = BTreeMap::new();
    let mut files: Vec<File> = vec![];
    let mut file_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut shown: BTreeMap<&str, String> = BTreeMap::new();
    for (name, p) in &cmd.params {
        if p.kind == ParamKind::File {
            if req.args.contains_key(name) {
                return Err(format!("--{name} is a file: give its path"));
            }
            let Some(f) = req.files.get(name) else {
                return Err(format!("{} needs --{name} ({})", cmd.name, usage(cmd)));
            };
            let max = p.max_bytes.unwrap_or(FILE_DEFAULT);
            if f.name.is_empty() || f.name.len() > 255 || f.name.contains('/') || !plain(&f.name) {
                return Err(format!("--{name}: the file's name is plain text without folders"));
            }
            let ext = p.extension.clone().unwrap_or_default();
            if !ext.is_empty() && !f.name.to_ascii_lowercase().ends_with(&ext.to_ascii_lowercase()) {
                return Err(format!("--{name} is a {ext} file"));
            }
            // A size check before decoding, from the encoded length.
            if (f.base64.len() as u64) / 4 * 3 > max + 3 {
                return Err(format!("--{name} is at most {max} bytes"));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(f.base64.as_bytes())
                .map_err(|e| format!("--{name}: unreadable base64 ({e})"))?;
            if bytes.len() as u64 > max {
                return Err(format!("--{name} is at most {max} bytes"));
            }
            let sha = hex::encode(sha2::Sha256::digest(&bytes));
            if !f.sha256.eq_ignore_ascii_case(&sha) {
                return Err(format!("--{name}: the bytes don't match the SHA-256 sent with them"));
            }
            shown.insert(name, format!("<file {} ({} bytes, sha256 {sha})>", f.name, bytes.len()));
            let i = match files.iter().position(|x| x.sha256 == sha && x.extension == ext) {
                Some(i) => i,
                None => {
                    files.push(File { bytes, sha256: sha, extension: ext });
                    files.len() - 1
                }
            };
            file_of.insert(name, i);
            continue;
        }
        if req.files.contains_key(name) {
            return Err(format!("--{name} isn't a file"));
        }
        let v = match (req.args.get(name), &p.default) {
            (Some(v), _) => v.clone(),
            (None, Some(d)) => d.clone(),
            (None, None) => return Err(format!("{} needs --{name} ({})", cmd.name, usage(cmd))),
        };
        check_value(name, p, &v)?;
        values.insert(name, v);
    }
    let mut argv = vec![];
    let mut shown_argv = vec![];
    for a in &cmd.run {
        let mut parts = vec![];
        let mut text = String::new();
        for piece in pieces(a)? {
            match piece {
                Piece::Lit(t) => {
                    text.push_str(&t);
                    parts.push(Arg::Text(t));
                }
                Piece::Param(n) => match file_of.get(n.as_str()) {
                    Some(i) => {
                        text.push_str(&shown[n.as_str()]);
                        parts.push(Arg::File(*i));
                    }
                    None => {
                        let v = values[n.as_str()].clone();
                        text.push_str(&v);
                        parts.push(Arg::Text(v));
                    }
                },
            }
        }
        argv.push(parts);
        shown_argv.push(text);
    }
    let detail = crate::gate::visible(&format!(
        "{} on {node} for {caller}:\n{}",
        cmd.name,
        shown_argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
    ));
    let hashed = json!({
        "command": cmd.name,
        "args": values,
        "files": file_of.iter().map(|(k, i)| (k.to_string(), files[*i].sha256.clone())).collect::<BTreeMap<_, _>>(),
    });
    let args_hash = crate::gate::args_hash(&cmd.name, &hashed);
    Ok(Checked { argv, files, detail, args_hash })
}

/// An argument as a person reads it: bare when it's plain, else in single quotes.
fn quote(a: &str) -> String {
    let bare = !a.is_empty()
        && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@+%<>()".contains(c))
        && !a.contains("<file");
    if bare || a.starts_with("<file ") && !a.contains('\'') {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

/// `--rack <staging|production> --script <file .rb> --why <text>`.
pub fn usage(cmd: &CommandConfig) -> String {
    let parts: Vec<String> = cmd
        .ordered()
        .into_iter()
        .map(|(n, p)| {
            let what = match p.kind {
                ParamKind::Choice => p.values.join("|"),
                ParamKind::Text => "text".into(),
                ParamKind::Pattern => format!("matching {}", p.pattern.as_deref().unwrap_or_default()),
                ParamKind::File => match &p.extension {
                    Some(e) => format!("{e} file"),
                    None => "file".into(),
                },
            };
            match &p.default {
                Some(d) => format!("[--{n} <{what}> (default {d})]"),
                None => format!("--{n} <{what}>"),
            }
        })
        .collect();
    if parts.is_empty() { "no parameters".into() } else { parts.join(" ") }
}

// ---------------------------------------------------------------- run counts, health

/// Runs of each command since the node started, published as `command.runs` readings.
#[derive(Default)]
pub struct Runs {
    state: std::sync::Mutex<BTreeMap<String, RunStats>>,
    changed: tokio::sync::Notify,
}

#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunStats {
    running: u64,
    runs: u64,
    refused: u64,
    failed: u64,
    last_exit: Option<i32>,
    last_outcome: Option<String>,
    last_caller: Option<String>,
    last_at: Option<jiff::Timestamp>,
}

impl Runs {
    fn update(&self, name: &str, f: impl FnOnce(&mut RunStats)) {
        f(self.state.lock().unwrap_or_else(|e| e.into_inner()).entry(name.to_string()).or_default());
        self.changed.notify_one();
    }

    fn refused(&self, name: &str, caller: &str) {
        self.update(name, |s| {
            s.refused += 1;
            s.last_outcome = Some("refused".into());
            s.last_caller = Some(caller.into());
            s.last_at = Some(jiff::Timestamp::now());
        });
    }

    pub fn readings(&self, commands: &[CommandConfig]) -> Vec<Value> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let at = jiff::Timestamp::now();
        commands
            .iter()
            .map(|c| {
                let stats = s.get(&c.name).cloned().unwrap_or_default();
                json!({"kind": "command.runs", "key": c.name, "at": at, "data": stats})
            })
            .collect()
    }
}

/// One run in the counts: running while it lives, its outcome when it ends.
struct Running<'a> {
    runs: &'a Runs,
    name: String,
    outcome: Option<(Option<i32>, String)>,
}

impl<'a> Running<'a> {
    fn start(runs: &'a Runs, name: &str, caller: &str) -> Self {
        runs.update(name, |s| {
            s.running += 1;
            s.runs += 1;
            s.last_caller = Some(caller.into());
            s.last_at = Some(jiff::Timestamp::now());
        });
        Running { runs, name: name.into(), outcome: None }
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let (code, outcome) = self.outcome.take().unwrap_or((None, "lost".into()));
        self.runs.update(&self.name, |s| {
            s.running = s.running.saturating_sub(1);
            if code != Some(0) {
                s.failed += 1;
            }
            s.last_exit = code;
            s.last_outcome = Some(outcome);
        });
    }
}

/// Publish each command's `command.runs` reading when its runs change (at most every 2 seconds), and every 10
/// minutes.
pub async fn report(node: Arc<Node>) {
    if node.commands.is_empty() {
        return;
    }
    loop {
        let readings = node.command_runs.readings(&node.commands);
        crate::readings::publish(&node, &json!({"readings": readings})).await;
        let _ = tokio::time::timeout(Duration::from_secs(600), node.command_runs.changed.notified()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// The node's health problems from its commands: a program that isn't there or can't be run (uninstalled, moved, not
/// on the node's PATH), so a command that would fail every run turns the heartbeat red before anyone needs it.
pub fn problems(node: &Node) -> Vec<String> {
    node.commands
        .iter()
        .filter_map(|c| {
            let program = c.run.first()?;
            match resolve_program(program) {
                Ok(_) => None,
                Err(e) => Some(format!("shared command {} can't run: {e}", c.name)),
            }
        })
        .collect()
}

/// The program's path: as given when it has a `/`, else found on the node's PATH. Must be an executable file.
fn resolve_program(program: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let is_exec = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if program.contains('/') {
        let p = PathBuf::from(program);
        return if is_exec(&p) { Ok(p) } else { Err(format!("{program} isn't an executable file")) };
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| is_exec(p))
        .ok_or_else(|| format!("{program} isn't on the node's PATH (give its full path in run:)"))
}

// ---------------------------------------------------------------- serving a command this node shares

/// The control plane asks this node to take one run of a command for a caller.
pub async fn serve(node: Arc<Node>, req: CommandServe) -> Result<Value, RpcError> {
    let cmd = node
        .commands
        .iter()
        .find(|c| c.name == req.command)
        .cloned()
        .ok_or_else(|| RpcError::not_here(format!("{} shares no command called {}", node.name, req.command)))?;
    if !cmd.who.iter().any(|p| crate::tools::glob(p, &req.caller)) {
        tracing::warn!(command = %cmd.name, caller = %req.caller, "refused a command run: not in its who: list");
        node.command_runs.refused(&cmd.name, &req.caller);
        return Err(RpcError::refused(format!("{} may not run {} on {}", req.caller, cmd.name, node.name)));
    }
    let ws = crate::tools::connect_stream(&node, &req.stream_id, "serve")
        .await
        .map_err(|e| RpcError::refused(e.to_string()))?;
    let (near, far) = tokio::io::duplex(256 * 1024);
    let (far_r, far_w) = tokio::io::split(far);
    tokio::spawn(crate::tools::pipe(far_r, far_w, ws));
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(near);
        take(node, cmd, req, r, w).await;
    });
    Ok(json!({}))
}

/// Lines to the caller.
struct Out<W> {
    w: W,
}

impl<W: AsyncWrite + Unpin> Out<W> {
    async fn send(&mut self, v: Value) -> bool {
        self.w.write_all(format!("{v}\n").as_bytes()).await.is_ok() && self.w.flush().await.is_ok()
    }
}

/// The largest request line a command can take: its files encoded, and room for the rest.
fn request_limit(cmd: &CommandConfig) -> u64 {
    let files: u64 = cmd
        .params
        .values()
        .filter(|p| p.kind == ParamKind::File)
        .map(|p| p.max_bytes.unwrap_or(FILE_DEFAULT).div_ceil(3) * 4 + 1024)
        .sum();
    files + 256 * 1024
}

/// Read the request, check it, ask the control plane, then run it.
async fn take<R, W>(node: Arc<Node>, cmd: CommandConfig, req: CommandServe, r: R, w: W)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut out = Out { w };
    let mut reader = BufReader::new(r);
    let mut line = Vec::new();
    let limit = request_limit(&cmd);
    let read = tokio::time::timeout(REQUEST_WAIT, (&mut reader).take(limit).read_until(b'\n', &mut line)).await;
    let refuse = |why: String| json!({"type": "refused", "reason": why});
    let request: RunRequest = match read {
        Ok(Ok(n)) if n > 0 && line.ends_with(b"\n") => match serde_json::from_slice(&line) {
            Ok(r) => r,
            Err(e) => {
                out.send(refuse(format!("unreadable request: {e}"))).await;
                return;
            }
        },
        Ok(Ok(n)) if n as u64 >= limit => {
            out.send(refuse(format!("the request is over {limit} bytes: a file is too big"))).await;
            return;
        }
        _ => {
            out.send(refuse("no request arrived".into())).await;
            return;
        }
    };
    drop(line);
    let checked = match check(&cmd, &request, &req.caller, &node.name) {
        Ok(c) => c,
        Err(why) => {
            tracing::info!(command = %cmd.name, caller = %req.caller, "refused a command run: {why}");
            node.command_runs.refused(&cmd.name, &req.caller);
            out.send(refuse(why)).await;
            return;
        }
    };
    drop(request);
    // The caller's later lines: a cancel, or the end of its side.
    let (cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(l)) => {
                    if serde_json::from_str::<Value>(&l).is_ok_and(|v| v["type"] == "cancel") {
                        let _ = cancel_tx.send(true);
                        return;
                    }
                }
                _ => {
                    let _ = cancel_tx.send(true);
                    return;
                }
            }
        }
    });
    let detail =
        crate::gate::scrub(&node, &checked.detail, &format!("command {} for {}", cmd.name, req.caller)).into_owned();
    let call = ToolCall {
        stream_id: req.stream_id.clone(),
        targets: vec![],
        args_hash: checked.args_hash.clone(),
        detail: Some(detail.clone()),
        justification: req.justification.clone(),
        peek: None,
    };
    match decide(&node, &call, cmd.approval_wait(), &mut out, &mut cancel).await {
        Ok(()) => {}
        Err(why) => {
            tracing::info!(command = %cmd.name, caller = %req.caller, "a command run wasn't allowed: {why}");
            node.command_runs.refused(&cmd.name, &req.caller);
            out.send(refuse(why)).await;
            return;
        }
    }
    let id = request_id(&req.stream_id);
    let dir = node.home.join(".local/state/fleetlayer/commands").join(&id);
    let _cleanup = Cleanup(dir.clone());
    if let Err(e) = write_files(&dir, &checked.files) {
        tracing::warn!(command = %cmd.name, "couldn't write a command's files: {e}");
        out.send(refuse(format!("{} couldn't write the files: {e}", node.name))).await;
        return;
    }
    let argv = checked.argv(&dir);
    tracing::info!(command = %cmd.name, caller = %req.caller, run = %id, detail = %detail, "running a shared command");
    let mut running = Running::start(&node.command_runs, &cmd.name, &req.caller);
    let (code, outcome) = run(&node, &cmd, &argv, &dir, &req.caller, &mut out, &mut cancel).await;
    tracing::info!(command = %cmd.name, caller = %req.caller, run = %id, ?code, outcome = %outcome, "a shared command ended");
    running.outcome = Some((code, outcome));
}

/// Removes a run's folder when the run ends, however it ends.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.0.exists()
            && let Err(e) = std::fs::remove_dir_all(&self.0)
        {
            tracing::warn!("couldn't remove {}: {e}", self.0.display());
        }
    }
}

/// A run's ID: its own folder's name.
fn request_id(stream_id: &str) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let h = sha2::Sha256::digest(format!("{stream_id}:{now}:{}", std::process::id()).as_bytes());
    hex::encode(&h[..12])
}

/// Write each file into a new folder only this user can open (0700), each file created new (0600, never through a
/// link), then read back and checked against its hash.
pub fn write_files(dir: &Path, files: &[File]) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let parent = dir.parent().ok_or_else(|| std::io::Error::other("no parent folder"))?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    // A new folder: fails if anything is already there.
    std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    for f in files {
        let path = dir.join(f.file_name());
        let mut file = create_private(&path)?;
        std::io::Write::write_all(&mut file, &f.bytes)?;
        file.sync_all()?;
        drop(file);
        let back = std::fs::read(&path)?;
        if hex::encode(sha2::Sha256::digest(&back)) != f.sha256 {
            return Err(std::io::Error::other(format!("{} changed after it was written", path.display())));
        }
    }
    Ok(())
}

/// A new file only this user can read (0600): never one already there, never through a link.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)
}

/// Ask the control plane about this run; while it waits for the person, tell the caller once and look again every
/// few seconds until `wait` is up. Fails closed: no answer is a refusal.
async fn decide<W: AsyncWrite + Unpin>(
    node: &Node,
    call: &ToolCall,
    wait: Duration,
    out: &mut Out<W>,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    let started = tokio::time::Instant::now();
    let ask = |c: ToolCall| async move {
        let peer = node.link.peer().await.ok_or_else(|| "not linked to it now".to_string())?;
        peer.request_within::<_, l::ToolCallDecision>(l::method::COMMAND_CALL, &c, Duration::from_secs(20))
            .await
            .map_err(|e| e.message)
    };
    let unreachable = |e: String| {
        format!("{} couldn't check this run with the control plane ({e}), so it didn't start it", node.name)
    };
    let mut d = ask(call.clone()).await.map_err(unreachable)?;
    let mut told = false;
    loop {
        match d.decision.as_str() {
            "allowed" => return Ok(()),
            "pending" if started.elapsed() + POLL < wait => {
                let id = d.approval.clone().unwrap_or_default();
                if !told {
                    told = true;
                    out.send(json!({"type": "waiting", "approval": id, "reason": d.reason})).await;
                }
                tokio::select! {
                    _ = tokio::time::sleep(POLL) => {}
                    _ = cancelled(cancel) => return Err("cancelled while it waited for approval".into()),
                }
                let peek = ToolCall { peek: d.approval.clone(), ..call.clone() };
                let looked = ask(peek).await.map_err(unreachable)?;
                d = match looked.decision.as_str() {
                    "ready" => ask(call.clone()).await.map_err(unreachable)?,
                    _ => looked,
                };
            }
            "pending" => {
                let id = d.approval.unwrap_or_default();
                return Err(format!(
                    "this run needs your person's approval (request {id}), and they haven't answered yet. Tell them \
                     what you're trying to do and that request {id} is waiting, then run it again with the same \
                     arguments once they've approved it"
                ));
            }
            _ => return Err(d.reason),
        }
    }
}

/// Start the program and stream its output until it ends, the caller cancels or the timeout. Answers its exit code
/// (None when it was stopped) and how it ended.
async fn run<W: AsyncWrite + Unpin>(
    node: &Node,
    cmd: &CommandConfig,
    argv: &[String],
    dir: &Path,
    caller: &str,
    out: &mut Out<W>,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> (Option<i32>, String) {
    let program = match resolve_program(&argv[0]) {
        Ok(p) => p,
        Err(e) => {
            out.send(json!({"type": "refused", "reason": format!("{} can't run it: {e}", node.name)})).await;
            return (None, "not started".into());
        }
    };
    let mut c = tokio::process::Command::new(&program);
    c.args(&argv[1..]).env_clear();
    for k in BASE_ENV.iter().copied().chain(cmd.env.iter().map(String::as_str)) {
        if let Some(v) = std::env::var_os(k) {
            c.env(k, v);
        }
    }
    c.env("FLEETLAYER_CALLER", caller);
    let cwd = match &cmd.cwd {
        Some(p) => match p.strip_prefix("~") {
            Ok(rest) => node.home.join(rest),
            Err(_) => p.clone(),
        },
        None => dir.to_path_buf(),
    };
    c.current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            out.send(json!({"type": "refused", "reason": format!("starting {}: {e}", argv[0])})).await;
            return (None, "not started".into());
        }
    };
    out.send(json!({"type": "started"})).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(&'static str, String)>(64);
    if let Some(o) = child.stdout.take() {
        tokio::spawn(lines("stdout", o, tx.clone()));
    }
    if let Some(e) = child.stderr.take() {
        tokio::spawn(lines("stderr", e, tx.clone()));
    }
    drop(tx);
    let max = cmd.max_output.unwrap_or(OUTPUT_DEFAULT);
    let mut sent: u64 = 0;
    let mut cut = false;
    let context = format!("output of command {} for {caller}", cmd.name);
    let deadline = tokio::time::sleep(cmd.timeout());
    tokio::pin!(deadline);
    let mut stop: Option<&'static str> = None;
    let mut open = true;
    let status = loop {
        tokio::select! {
            line = rx.recv(), if open => match line {
                Some((stream, text)) => {
                    if cut {
                        continue;
                    }
                    if sent + text.len() as u64 > max {
                        cut = true;
                        out.send(json!({"type": "note", "text": format!("output past {max} bytes isn't sent; the command runs on")})).await;
                        continue;
                    }
                    sent += text.len() as u64;
                    let text = crate::gate::scrub(node, &text, &context).into_owned();
                    out.send(json!({"type": "output", "stream": stream, "text": text})).await;
                }
                None => open = false,
            },
            s = child.wait(), if !open => break s.ok(),
            _ = &mut deadline, if stop.is_none() => {
                stop = Some("timeout");
                terminate(&mut child).await;
            }
            _ = cancelled(cancel), if stop.is_none() => {
                stop = Some("cancelled");
                terminate(&mut child).await;
            }
        }
    };
    use std::os::unix::process::ExitStatusExt;
    let code = status.and_then(|s| s.code());
    let signal = status.and_then(|s| s.signal());
    match (stop, code) {
        (Some(why), _) => {
            let reason = match why {
                "timeout" => format!("stopped after its timeout of {}s", cmd.timeout().as_secs()),
                _ => "stopped: the caller cancelled it".into(),
            };
            out.send(json!({"type": "exit", "signal": signal, "stopped": why, "reason": reason})).await;
            (None, why.into())
        }
        (None, Some(c)) => {
            out.send(json!({"type": "exit", "code": c})).await;
            (Some(c), "exited".into())
        }
        (None, None) => {
            out.send(
                json!({"type": "exit", "signal": signal, "reason": format!("ended by signal {}", signal.unwrap_or(0))}),
            )
            .await;
            (None, "signal".into())
        }
    }
}

/// Resolves once the caller has cancelled (or gone). The watch's guard is dropped here, never held across an await.
async fn cancelled(cancel: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = cancel.wait_for(|c| *c).await;
}

/// SIGTERM to the program's process group, then SIGKILL after a grace period. Its output keeps flowing meanwhile.
async fn terminate(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else { return };
    let group = nix::unistd::Pid::from_raw(-(pid as i32));
    let _ = nix::sys::signal::kill(group, nix::sys::signal::Signal::SIGTERM);
    tokio::spawn(async move {
        tokio::time::sleep(GRACE).await;
        let _ = nix::sys::signal::kill(group, nix::sys::signal::Signal::SIGKILL);
    });
}

/// A pipe's output as lines (a long line in pieces), as text.
async fn lines<R: AsyncRead + Unpin>(
    stream: &'static str,
    r: R,
    tx: tokio::sync::mpsc::Sender<(&'static str, String)>,
) {
    let mut r = BufReader::new(r);
    loop {
        let mut buf = Vec::new();
        match (&mut r).take(LINE_MAX as u64).read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if tx.send((stream, String::from_utf8_lossy(&buf).into_owned())).await.is_err() {
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------- running a command another node shares

/// The first line a local caller (`fleetlayer run`) writes on the node's tools socket.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRequest {
    /// `<name>@<node>`.
    pub command: String,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
}

/// Open a stream to the command for a local caller: answers `{"ok":true}` or `{"error":…}`, then carries the run's
/// lines both ways.
pub async fn handle<R, W>(node: Arc<Node>, req: OpenRequest, reader: R, mut writer: W)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    match open(&node, &req).await {
        Ok(ws) => {
            if writer.write_all(b"{\"ok\":true}\n").await.is_ok() {
                crate::tools::pipe(reader, writer, ws).await;
            }
        }
        Err(e) => {
            let msg = json!({"error": e}).to_string();
            let _ = writer.write_all(format!("{msg}\n").as_bytes()).await;
        }
    }
}

async fn open(
    node: &Node,
    req: &OpenRequest,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, String> {
    let Some((command, on)) = req.command.rsplit_once('@') else {
        return Err(format!("{}: name the command and its node, <command>@<node>", req.command));
    };
    let caller = crate::tools::caller_name(node, req.session_id.as_deref(), req.address.as_deref()).await;
    let peer = node.link.peer().await.ok_or_else(|| format!("{} isn't linked to a control plane now", node.name))?;
    let opened: ToolOpened = peer
        .request_within(
            l::method::COMMAND_OPEN,
            &CommandOpen { command: command.into(), node: on.into(), caller, justification: req.why.clone() },
            Duration::from_secs(30),
        )
        .await
        .map_err(|e| e.message)?;
    crate::tools::connect_stream(node, &opened.stream_id, "use").await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rack() -> CommandConfig {
        yaml_serde::from_str(
            r#"
name: rack-gateway-run
description: Run a script on a rack
run: [/usr/bin/rack-gateway, run, --rack, "{rack}", "--script={script}", --justification, "{why_text}", "--n={n}"]
params:
  rack: { type: choice, values: [staging, production] }
  script: { type: file, max_bytes: 64, extension: .rb }
  why_text: { type: text, max_length: 20 }
  n: { type: pattern, pattern: "[0-9]+", default: "1" }
who: ["*@agents"]
timeout: 30m
"#,
        )
        .unwrap()
    }

    fn file(name: &str, bytes: &[u8]) -> FileArg {
        FileArg {
            name: name.into(),
            sha256: hex::encode(sha2::Sha256::digest(bytes)),
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    fn req(args: &[(&str, &str)], files: Vec<(&str, FileArg)>) -> RunRequest {
        RunRequest {
            kind: "run".into(),
            args: args.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            files: files.into_iter().map(|(k, f)| (k.to_string(), f)).collect(),
        }
    }

    #[test]
    fn a_good_config_passes_and_bad_ones_say_why() {
        let c = rack();
        c.check().unwrap();
        assert_eq!(c.timeout(), Duration::from_secs(1800));
        let bad = |f: &dyn Fn(&mut CommandConfig), want: &str| {
            let mut c = rack();
            f(&mut c);
            let e = c.check().unwrap_err();
            assert!(e.contains(want), "{e} (wanted {want})");
        };
        bad(&|c| c.run[0] = "{rack}".into(), "the program");
        bad(&|c| c.run.push("{nope}".into()), "isn't one of its params");
        bad(&|c| c.run.push("{unclosed".into()), "without its }");
        bad(&|c| c.run.push("x}".into()), "without its {");
        bad(&|c| c.name = "bad name".into(), "a name is");
        bad(
            &|c| {
                c.params.insert("why".into(), c.params["why_text"].clone());
                c.run.push("{why}".into());
            },
            "--why",
        );
        bad(
            &|c| {
                c.params.insert("unused".into(), c.params["why_text"].clone());
            },
            "not used",
        );
        bad(&|c| c.params.get_mut("rack").unwrap().values.clear(), "lists its values");
        bad(&|c| c.params.get_mut("n").unwrap().pattern = Some("(".into()), "pattern:");
        bad(&|c| c.params.get_mut("n").unwrap().default = Some("x".into()), "default:");
        bad(&|c| c.params.get_mut("script").unwrap().default = Some("x".into()), "no default");
        bad(&|c| c.params.get_mut("script").unwrap().max_bytes = Some(FILE_MAX + 1), "max_bytes");
        bad(&|c| c.params.get_mut("script").unwrap().extension = Some("rb".into()), "extension");
        bad(&|c| c.timeout = Some(Duration_(TIMEOUT_MAX + 1)), "timeout");
        bad(&|c| c.env = vec!["A B".into()], "env:");
        // Literal braces.
        let mut c = rack();
        c.run.push("{{literal}}".into());
        c.check().unwrap();
    }

    #[test]
    fn info_names_the_params_in_order_and_never_the_program() {
        let i = rack().info();
        let names: Vec<&str> = i.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["rack", "script", "why_text", "n"]);
        assert_eq!(i.params[1].max_bytes, Some(64));
        assert_eq!(i.params[2].max_length, Some(20));
        assert_eq!(i.timeout, 1800);
        assert!(!serde_json::to_string(&i).unwrap().contains("/usr/bin"), "the program isn't shared");
    }

    #[test]
    fn a_good_request_becomes_an_argument_list_with_the_file_path() {
        let c = rack();
        let r =
            req(&[("rack", "staging"), ("why_text", "fix the thing")], vec![("script", file("fix.rb", b"puts 1\n"))]);
        let k = check(&c, &r, "ci@agents", "laptop").unwrap();
        let dir = Path::new("/state/run1");
        let sha = hex::encode(sha2::Sha256::digest(b"puts 1\n"));
        assert_eq!(
            k.argv(dir),
            vec![
                "/usr/bin/rack-gateway".to_string(),
                "run".into(),
                "--rack".into(),
                "staging".into(),
                format!("--script=/state/run1/{sha}.rb"),
                "--justification".into(),
                "fix the thing".into(),
                "--n=1".into(),
            ]
        );
        assert!(
            k.detail.starts_with("rack-gateway-run on laptop for ci@agents:\n/usr/bin/rack-gateway run"),
            "{}",
            k.detail
        );
        assert!(k.detail.contains(&format!("<file fix.rb (7 bytes, sha256 {sha})>")), "{}", k.detail);
        assert!(k.detail.contains("'fix the thing'"), "{}", k.detail);
        assert!(!k.detail.contains("puts 1"), "never the file's contents");
        // The same arguments hash the same; any change changes it.
        let again = check(&c, &r, "ci@agents", "laptop").unwrap();
        assert_eq!(k.args_hash, again.args_hash);
        let other = req(
            &[("rack", "production"), ("why_text", "fix the thing")],
            vec![("script", file("fix.rb", b"puts 1\n"))],
        );
        assert_ne!(k.args_hash, check(&c, &other, "ci@agents", "laptop").unwrap().args_hash);
        let other_file =
            req(&[("rack", "staging"), ("why_text", "fix the thing")], vec![("script", file("fix.rb", b"puts 2\n"))]);
        assert_ne!(k.args_hash, check(&c, &other_file, "ci@agents", "laptop").unwrap().args_hash);
    }

    #[test]
    fn anything_undeclared_missing_or_off_type_is_refused() {
        let c = rack();
        let ok_file = || vec![("script", file("fix.rb", b"x"))];
        let refused = |r: RunRequest, want: &str| {
            let e = check(&c, &r, "ci@agents", "laptop").unwrap_err();
            assert!(e.contains(want), "{e} (wanted {want})");
        };
        refused(req(&[("rack", "staging"), ("why_text", "x"), ("extra", "1")], ok_file()), "takes no --extra");
        refused(req(&[("why_text", "x")], ok_file()), "needs --rack");
        refused(req(&[("rack", "staging"), ("why_text", "x")], vec![]), "needs --script");
        refused(req(&[("rack", "prod"), ("why_text", "x")], ok_file()), "one of staging, production");
        refused(req(&[("rack", "staging"), ("why_text", "x".repeat(21).as_str())], ok_file()), "at most 20");
        refused(req(&[("rack", "staging"), ("why_text", "a\nb")], ok_file()), "control characters");
        refused(req(&[("rack", "staging"), ("why_text", "a\u{202e}b")], ok_file()), "hide themselves");
        refused(req(&[("rack", "staging"), ("why_text", "--rack=production")], ok_file()), "can't start with -");
        refused(req(&[("rack", "staging"), ("why_text", "x"), ("n", "1; rm")], ok_file()), "must match");
        refused(req(&[("rack", "staging"), ("why_text", "x"), ("n", "12\n")], ok_file()), "control");
        refused(req(&[("rack", "staging"), ("why_text", "x"), ("script", "/etc/passwd")], ok_file()), "is a file");
        refused(req(&[("rack", "staging"), ("why_text", "x")], vec![("script", file("fix.sh", b"x"))]), "a .rb file");
        refused(
            req(&[("rack", "staging"), ("why_text", "x")], vec![("script", file("../fix.rb", b"x"))]),
            "without folders",
        );
        refused(
            req(&[("rack", "staging"), ("why_text", "x")], vec![("script", file("big.rb", &[b'a'; 65]))]),
            "at most 64 bytes",
        );
        let mut lying = file("fix.rb", b"x");
        lying.sha256 = hex::encode(sha2::Sha256::digest(b"y"));
        refused(req(&[("rack", "staging"), ("why_text", "x")], vec![("script", lying)]), "don't match");
        refused(
            req(&[("rack", "staging")], vec![("script", file("a.rb", b"x")), ("why_text", file("a.rb", b"x"))]),
            "isn't a file",
        );
        let mut r = req(&[], vec![]);
        r.kind = "other".into();
        refused(r, "expected a run request");
        // Upper-case extension is the same extension; the value of a choice stays one argument whatever it holds.
        check(&c, &req(&[("rack", "staging"), ("why_text", "x")], vec![("script", file("FIX.RB", b"x"))]), "a@b", "n")
            .unwrap();
    }

    #[test]
    fn files_go_in_a_new_private_folder_and_never_through_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commands/run1");
        let f = File {
            bytes: b"echo hi\n".to_vec(),
            sha256: hex::encode(sha2::Sha256::digest(b"echo hi\n")),
            extension: ".sh".into(),
        };
        write_files(&dir, std::slice::from_ref(&f)).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let path = dir.join(f.file_name());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"echo hi\n");
        // The folder must be new.
        assert!(write_files(&dir, &[]).is_err());
        // A link planted where a file goes isn't followed: the run fails, and what it pointed at is untouched.
        let dir2 = tmp.path().join("commands/run2");
        std::fs::create_dir(&dir2).unwrap();
        let target = tmp.path().join("victim");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, dir2.join(f.file_name())).unwrap();
        assert!(create_private(&dir2.join(f.file_name())).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn durations_take_units() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("30m").unwrap(), 1800);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert!(parse_duration("2w").is_err());
        assert!(parse_duration("m").is_err());
    }

    #[test]
    fn arguments_are_shown_quoted_when_they_need_it() {
        assert_eq!(quote("--rack"), "--rack");
        assert_eq!(quote("fix the thing"), "'fix the thing'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("$(x)"), "'$(x)'");
    }

    /// A real run: a program started with exactly its arguments (no shell), its output streamed as lines, its exit
    /// code; a timeout stops it; a cancel stops it.
    #[tokio::test]
    async fn a_run_streams_output_and_ends_with_its_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let node = Node::for_test(tmp.path());
        let cmd: CommandConfig = yaml_serde::from_str(
            "name: t\nrun: [/bin/sh, -c, 'echo out; echo err >&2; echo \"$1\"; exit 3', sh, '{arg}']\nparams:\n  arg: { type: text }\nwho: ['*']\n",
        )
        .unwrap();
        cmd.check().unwrap();
        let (mut w, r) = tokio::io::duplex(64 * 1024);
        let mut out = Out { w: &mut w };
        let (_tx, mut cancel) = tokio::sync::watch::channel(false);
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo out; echo err >&2; echo \"$1\"; exit 3".into(),
            "sh".into(),
            "$(touch /tmp/no); x".into(),
        ];
        let (code, how) = run(&node, &cmd, &argv, tmp.path(), "a@b", &mut out, &mut cancel).await;
        assert_eq!((code, how.as_str()), (Some(3), "exited"));
        drop(w);
        let mut text = String::new();
        BufReader::new(r).read_to_string(&mut text).await.unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0]["type"], "started");
        let outs: Vec<(String, String)> = lines
            .iter()
            .filter(|l| l["type"] == "output")
            .map(|l| (l["stream"].as_str().unwrap().into(), l["text"].as_str().unwrap().into()))
            .collect();
        assert!(outs.contains(&("stdout".into(), "out\n".into())), "{outs:?}");
        assert!(outs.contains(&("stderr".into(), "err\n".into())), "{outs:?}");
        assert!(
            outs.contains(&("stdout".into(), "$(touch /tmp/no); x\n".into())),
            "an argument is never run: {outs:?}"
        );
        assert_eq!(lines.last().unwrap()["code"], 3);

        // The timeout stops it.
        let slow: CommandConfig =
            yaml_serde::from_str("name: s\nrun: [/bin/sleep, '30']\nwho: ['*']\ntimeout: 1s\n").unwrap();
        let (mut w, _r) = tokio::io::duplex(64 * 1024);
        let mut out = Out { w: &mut w };
        let started = std::time::Instant::now();
        let (code, how) =
            run(&node, &slow, &["/bin/sleep".into(), "30".into()], tmp.path(), "a@b", &mut out, &mut cancel).await;
        assert_eq!((code, how.as_str()), (None, "timeout"));
        assert!(started.elapsed() < Duration::from_secs(10));

        // So does a cancel.
        let long: CommandConfig = yaml_serde::from_str("name: s\nrun: [/bin/sleep, '30']\nwho: ['*']\n").unwrap();
        let (tx, mut cancel) = tokio::sync::watch::channel(false);
        let (mut w, _r) = tokio::io::duplex(64 * 1024);
        let mut out = Out { w: &mut w };
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = tx.send(true);
        });
        let (code, how) =
            run(&node, &long, &["/bin/sleep".into(), "30".into()], tmp.path(), "a@b", &mut out, &mut cancel).await;
        assert_eq!((code, how.as_str()), (None, "cancelled"));
    }

    #[test]
    fn a_missing_program_is_a_health_problem() {
        assert!(resolve_program("/bin/sh").is_ok());
        assert!(resolve_program("sh").is_ok());
        assert!(resolve_program("/nonexistent/rack-gateway").unwrap_err().contains("isn't an executable"));
        assert!(resolve_program("no-such-program-here").unwrap_err().contains("PATH"));
    }
}
