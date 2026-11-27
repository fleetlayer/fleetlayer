//! fleetlayer-folders: a read-only MCP server for a few chosen folders, such as your screenshots and downloads, so
//! agents on other machines can pick up the screenshot you just took. A node shares it as a tool (`docs/plugins/folders.md`):
//!
//! ```yaml
//! tools:
//!   - name: folders
//!     command: fleetlayer-folders
//!     args: ["--folder", "screenshots=~/Screenshots", "--folder", "downloads=~/Downloads"]
//!     allow: ["*@server"]
//! ```
//!
//! Agents use it with `fleetlayer mcp folders@<node>`. Tools: `folders`, `list`, `latest`, `find` and `read`. Only
//! files directly in each folder are reachable: no subfolders, hidden files or symbolic links, and nothing is ever
//! written. MCP over stdio, one JSON-RPC message per line.

mod folders;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::SystemTime;

use clap::Parser;
use folders::{Folder, Kind, ReadOptions, Refusal};
use serde_json::{Value, json};

/// The protocol versions this server speaks, newest first.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// The largest request line read from the caller.
const MAX_REQUEST: usize = 256 * 1024;
/// A reply line becomes one WebSocket message on the way to the caller, and those are refused over 16 MiB: base64
/// adds a third, so a reply carries at most 10 MiB of file.
const HARD_MAX_READ_MB: u64 = 10;

#[derive(Parser, Debug)]
#[command(name = "fleetlayer-folders", version, about = "A read-only MCP server for chosen folders")]
struct Args {
    /// A folder agents may read, as name=path (`screenshots=~/Screenshots`). Repeat for each folder.
    #[arg(long = "folder", value_name = "NAME=PATH", required = true)]
    folders: Vec<String>,
    /// The most megabytes of one file a reply carries (at most 10).
    #[arg(long, default_value_t = 8)]
    max_read_mb: u64,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let folders = parse_folders(&args.folders, home.as_deref())?;
    let server = Server { folders, max_bytes: args.max_read_mb.clamp(1, HARD_MAX_READ_MB) * 1024 * 1024 };
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut reader = std::io::BufReader::new(stdin);
    loop {
        let line = match read_line(&mut reader)? {
            Line::Eof => return Ok(()),
            Line::TooLong => {
                send(&mut stdout, &error_reply(Value::Null, -32600, "the request is too large"))?;
                continue;
            }
            Line::Text(l) => l,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle(&line) {
            send(&mut stdout, &reply)?;
        }
    }
}

enum Line {
    Text(String),
    TooLong,
    Eof,
}

/// One line, at most `MAX_REQUEST` bytes: a longer one is read to its end and dropped.
fn read_line(r: &mut impl BufRead) -> std::io::Result<Line> {
    let mut buf = Vec::new();
    let mut too_long = false;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok(if buf.is_empty() && !too_long { Line::Eof } else { finish(buf, too_long) });
        }
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !too_long {
            buf.extend_from_slice(&chunk[..take]);
            if buf.len() > MAX_REQUEST {
                too_long = true;
                buf.clear();
            }
        }
        r.consume(take);
        if done {
            return Ok(finish(buf, too_long));
        }
    }
}

fn finish(buf: Vec<u8>, too_long: bool) -> Line {
    if too_long { Line::TooLong } else { Line::Text(String::from_utf8_lossy(&buf).into_owned()) }
}

fn send(out: &mut impl Write, v: &Value) -> std::io::Result<()> {
    let mut s = serde_json::to_string(v).unwrap_or_default();
    s.push('\n');
    out.write_all(s.as_bytes())?;
    out.flush()
}

/// `name=path` pairs; `~` is the home directory. Names are short words, unique.
fn parse_folders(specs: &[String], home: Option<&std::path::Path>) -> anyhow::Result<Vec<Folder>> {
    let mut out: Vec<Folder> = Vec::new();
    for spec in specs {
        let (name, path) = spec.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("--folder {spec:?}: expected name=path, such as screenshots=~/Screenshots")
        })?;
        let name = name.trim();
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 40
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "--folder {spec:?}: the name must be letters, digits, - or _"
        );
        anyhow::ensure!(!out.iter().any(|f| f.name == name), "--folder {name} is given twice");
        let path = match path.strip_prefix("~") {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                let home =
                    home.ok_or_else(|| anyhow::anyhow!("--folder {spec:?}: HOME isn't set, so ~ can't be expanded"))?;
                home.join(rest.trim_start_matches('/'))
            }
            _ => PathBuf::from(path),
        };
        anyhow::ensure!(path.is_absolute(), "--folder {spec:?}: the path must be absolute or start with ~");
        out.push(Folder { name: name.to_string(), path });
    }
    Ok(out)
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

struct Server {
    folders: Vec<Folder>,
    max_bytes: u64,
}

impl Server {
    /// Answer one JSON-RPC line; notifications get no reply.
    fn handle(&self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return Some(error_reply(Value::Null, -32700, "not JSON")),
        };
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or_default();
        let id = match id {
            Some(id) if !id.is_null() => id,
            // A notification (initialized, cancelled) or a reply to something we never asked: nothing to say.
            _ => return None,
        };
        let result = match method {
            "initialize" => self.initialize(&msg["params"]),
            "ping" => json!({}),
            "tools/list" => json!({"tools": tool_list()}),
            "tools/call" => self.call(&msg["params"]),
            "" => return Some(error_reply(id, -32600, "no method")),
            other => return Some(error_reply(id, -32601, &format!("unknown method {other}"))),
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params["protocolVersion"].as_str().unwrap_or_default();
        let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
        let names: Vec<&str> = self.folders.iter().map(|f| f.name.as_str()).collect();
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fleetlayer-folders", "version": env!("CARGO_PKG_VERSION")},
            "instructions": format!(
                "Read-only access to these folders on another machine: {}. `latest` finds the newest files (the \
                 screenshot just taken: latest with folder screenshots, read true), `read` returns a file: images \
                 you can see, text as text.",
                names.join(", ")
            ),
        })
    }

    fn call(&self, params: &Value) -> Value {
        let name = params["name"].as_str().unwrap_or_default();
        let args = &params["arguments"];
        let out = match name {
            "folders" => Ok(self.list_folders()),
            "list" => self.list(args),
            "latest" => self.latest(args),
            "find" => self.find(args),
            "read" => self.read(args),
            other => Err(Refusal(format!("there's no tool called {other:?}: use folders, list, latest, find or read"))),
        };
        match out {
            Ok(content) => json!({"content": content}),
            Err(r) => json!({"content": [{"type": "text", "text": r.0}], "isError": true}),
        }
    }

    fn folder(&self, args: &Value) -> Result<&Folder, Refusal> {
        let names = || self.folders.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ");
        match args["folder"].as_str() {
            Some(n) => self
                .folders
                .iter()
                .find(|f| f.name == n)
                .ok_or_else(|| Refusal(format!("there's no folder called {n:?} here; the folders are {}", names()))),
            None if self.folders.len() == 1 => Ok(&self.folders[0]),
            None => Err(Refusal(format!("say which folder: {}", names()))),
        }
    }

    fn list_folders(&self) -> Vec<Value> {
        let folders: Vec<Value> = self
            .folders
            .iter()
            .map(|f| match f.scan() {
                Ok(l) => json!({"name": f.name, "path": f.path, "files": l.total,
                                "newest": l.files.first().map(|e| json!({"name": e.name, "modified": e.modified, "ago": e.ago}))}),
                Err(r) => json!({"name": f.name, "path": f.path, "error": r.0}),
            })
            .collect();
        vec![text_json(&json!({ "folders": folders }))]
    }

    fn list(&self, args: &Value) -> Result<Vec<Value>, Refusal> {
        let folder = self.folder(args)?;
        let limit = uint(args, "limit", 50)?.clamp(1, 200);
        let offset = uint(args, "offset", 0)?;
        let mut l = folder.scan()?;
        l.files = l.files.into_iter().skip(offset).take(limit).collect();
        Ok(vec![text_json(&json!({"folder": folder.name, "listing": l}))])
    }

    fn latest(&self, args: &Value) -> Result<Vec<Value>, Refusal> {
        let folder = self.folder(args)?;
        let read = args["read"].as_bool().unwrap_or(false);
        let n = uint(args, "n", 1)?.clamp(1, if read { 5 } else { 20 });
        let kind = match args["kind"].as_str().unwrap_or("any") {
            "any" => None,
            "image" => Some(Kind::Image),
            "text" => Some(Kind::Text),
            "pdf" => Some(Kind::Pdf),
            "video" => Some(Kind::Video),
            "archive" => Some(Kind::Archive),
            other => return Err(Refusal(format!("kind {other:?}: use any, image, text, pdf, video or archive"))),
        };
        let mut l = folder.scan()?;
        l.files.retain(|e| kind.is_none_or(|k| e.kind == k));
        l.total = l.files.len();
        l.files.truncate(n);
        if l.files.is_empty() {
            return Err(Refusal(format!(
                "{} has no {}files",
                folder.name,
                kind.map(|k| format!("{} ", serde_json::to_value(k).unwrap_or_default().as_str().unwrap_or("")))
                    .unwrap_or_default()
            )));
        }
        let mut out = vec![text_json(&json!({"folder": folder.name, "listing": l}))];
        if read {
            // All the files together stay under the reply cap.
            let mut left = self.max_bytes;
            for e in &l.files {
                let opts = ReadOptions { max_px: max_px(args)?, max_bytes: left };
                match folders::read(folder, &e.name, &opts) {
                    Ok(items) => {
                        left = left.saturating_sub(content_bytes(&items));
                        out.extend(items);
                    }
                    Err(r) => out.push(json!({"type": "text", "text": format!("{}: {}", e.name, r.0)})),
                }
                if left == 0 {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn find(&self, args: &Value) -> Result<Vec<Value>, Refusal> {
        let folder = self.folder(args)?;
        let pattern = args["pattern"].as_str().filter(|p| !p.is_empty());
        let since = match args["since"].as_str() {
            Some(s) => Some(folders::parse_since(s, SystemTime::now())?),
            None => None,
        };
        if pattern.is_none() && since.is_none() {
            return Err(Refusal("give a pattern, a since, or both".into()));
        }
        let limit = uint(args, "limit", 50)?.clamp(1, 200);
        let mut l = folder.scan()?;
        l.files.retain(|e| {
            pattern.is_none_or(|p| folders::name_matches(p, &e.name)) && since.is_none_or(|s| e.mtime >= s)
        });
        l.total = l.files.len();
        l.files.truncate(limit);
        Ok(vec![text_json(&json!({"folder": folder.name, "listing": l}))])
    }

    fn read(&self, args: &Value) -> Result<Vec<Value>, Refusal> {
        let folder = self.folder(args)?;
        let name = args["name"].as_str().ok_or_else(|| Refusal("name is missing: the file to read".into()))?;
        folders::read(folder, name, &ReadOptions { max_px: max_px(args)?, max_bytes: self.max_bytes })
    }
}

fn uint(args: &Value, key: &str, default: usize) -> Result<usize, Refusal> {
    match &args[key] {
        Value::Null => Ok(default),
        v => v.as_u64().map(|n| n as usize).ok_or_else(|| Refusal(format!("{key} must be a whole number"))),
    }
}

fn max_px(args: &Value) -> Result<u32, Refusal> {
    Ok(uint(args, "max_px", folders::DEFAULT_MAX_PX as usize)?.min(20_000) as u32)
}

/// About how many bytes of file the items carry (base64 is 4/3 of it).
fn content_bytes(items: &[Value]) -> u64 {
    items
        .iter()
        .map(|i| {
            let s = i["data"].as_str().or(i["resource"]["blob"].as_str()).or(i["text"].as_str()).unwrap_or("");
            s.len() as u64 * 3 / 4
        })
        .sum()
}

fn text_json(v: &Value) -> Value {
    json!({"type": "text", "text": serde_json::to_string_pretty(v).unwrap_or_default()})
}

fn tool_list() -> Value {
    let folder = json!({"type": "string", "description": "The folder's name, from the folders tool (such as screenshots or downloads). May be left out when there's only one."});
    let ro = json!({"readOnlyHint": true, "openWorldHint": false});
    let max_px = json!({"type": "integer", "minimum": 0, "description": "Images are downscaled to this many pixels on their longest side (default 1568); 0 sends the original."});
    json!([
        {"name": "folders", "description": "The folders you can read, each with its file count and newest file.",
         "inputSchema": {"type": "object", "properties": {}}, "annotations": ro},
        {"name": "list", "description": "A folder's files, newest first: name, size, modified time, kind.",
         "inputSchema": {"type": "object", "properties": {
            "folder": folder,
            "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Default 50."},
            "offset": {"type": "integer", "minimum": 0}}}, "annotations": ro},
        {"name": "latest", "description": "The newest file(s) in a folder, such as the screenshot just taken. With read: true, their contents too (images you can see).",
         "inputSchema": {"type": "object", "properties": {
            "folder": folder,
            "n": {"type": "integer", "minimum": 1, "maximum": 20, "description": "How many (default 1; at most 5 with read)."},
            "kind": {"type": "string", "enum": ["any", "image", "text", "pdf", "video", "archive"]},
            "read": {"type": "boolean", "description": "Include the files' contents."},
            "max_px": max_px}}, "annotations": ro},
        {"name": "find", "description": "Files in a folder by name and/or time, newest first.",
         "inputSchema": {"type": "object", "properties": {
            "folder": folder,
            "pattern": {"type": "string", "description": "Case-insensitive; * and ? as wildcards, or a plain piece of the name."},
            "since": {"type": "string", "description": "30m, 2h, 3d, 1w, a date (2026-11-27) or a timestamp."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 200}}}, "annotations": ro},
        {"name": "read", "description": "Read one file: images come back as images you can see (downscaled), text as text, anything else as base64 data (up to the size cap).",
         "inputSchema": {"type": "object", "required": ["name"], "properties": {
            "folder": folder,
            "name": {"type": "string", "description": "The file's name, from list, latest or find."},
            "max_px": max_px}}, "annotations": ro},
    ])
}

#[cfg(test)]
mod tests;
