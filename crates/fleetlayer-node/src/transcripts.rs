//! Transcripts: part of a session's transcript file, read for a caller on this node or another, so a client elsewhere
//! can follow a session's conversation (herdr-fleet copies other machines' Claude Code transcripts for a phone's
//! Conversation view). The session's harness plugin says where its transcript is (`_fleetlayer.dev/harness/transcript`);
//! the node reads the bytes. A caller on another node goes through the control plane, which checks `read` on the
//! session (`link/transcript_read` → `link/transcript_serve`).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use fleetlayer_protocol::ext;
use fleetlayer_protocol::link::{
    self as l, TRANSCRIPT_CHUNK_MAX, TRANSCRIPT_TAIL_MAX, TranscriptChunk, TranscriptRange,
};
use fleetlayer_protocol::rpc::RpcError;
use serde_json::{Value, json};

use crate::Node;

/// One of this node's sessions' transcripts, the part `range` names.
pub async fn read_here(node: &Node, name: &str, range: &TranscriptRange) -> Result<TranscriptChunk, RpcError> {
    if let Some(a) = &range.agent_id
        && (!(1..=64).contains(&a.len()) || !a.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(RpcError::invalid_params("an agent ID is hex digits"));
    }
    let ask = |id: &str| json!({"sessionId": id, "agentId": range.agent_id});
    let (session_id, v): (String, Value) = match crate::deliver::resolve(node, name).await {
        Ok(session) => {
            let none = || {
                RpcError::refused(format!("{name} on {} has no harness plugin that knows its transcript", node.name))
            };
            let plugin_name = session.harness_plugin.clone().ok_or_else(none)?;
            let plugin = node.plugin(&plugin_name).ok_or_else(none)?;
            let peer =
                plugin.peer().await.ok_or_else(|| RpcError::refused(format!("plugin {plugin_name} isn't running")))?;
            let id = session.session_id;
            let v = peer.request_within(ext::method::HARNESS_TRANSCRIPT, &ask(&id), Duration::from_secs(10)).await?;
            (id, v)
        }
        // A session that ended (not listed any more, as a transcript search finds them): its ID, asked of every
        // harness plugin that knows transcripts.
        Err(e) if !is_session_id(name) => return Err(e),
        Err(e) => {
            let mut found = None;
            for p in &node.plugins {
                if !p.roles().await.harness.is_some_and(|h| h.transcripts) {
                    continue;
                }
                let Some(peer) = p.peer().await else { continue };
                if let Ok(v) = peer
                    .request_within::<_, Value>(ext::method::HARNESS_TRANSCRIPT, &ask(name), Duration::from_secs(10))
                    .await
                {
                    found = Some(v);
                    break;
                }
            }
            (name.to_string(), found.ok_or(e)?)
        }
    };
    let path = v["path"].as_str().ok_or_else(|| RpcError::internal("the harness plugin named no transcript path"))?;
    let format = v["format"].as_str().unwrap_or("jsonl").to_string();
    let (path, range2) = (path.to_string(), range.clone());
    // Each line is redacted whole before it's cut, so a cut never leaves half a secret behind.
    let scrubber = node.secrets.scrubber();
    let (read, hits) = tokio::task::spawn_blocking(move || {
        let mut hits = vec![];
        let read = read_range(Path::new(&path), &range2, &mut |line: &[u8]| {
            let text = String::from_utf8_lossy(line);
            let (out, h) = scrubber.scrub(&text);
            hits.extend(h);
            out.into_owned().into_bytes()
        });
        (read, hits)
    })
    .await
    .map_err(|e| RpcError::internal(e.to_string()))?;
    node.secrets.report(&hits, &format!("the transcript of {name} on {}", node.name));
    let read = read.map_err(|e| RpcError::refused(format!("reading {name}'s transcript: {e}")))?;
    Ok(TranscriptChunk {
        session_id,
        format,
        size: read.size,
        offset: read.offset,
        next: read.next,
        data: base64::engine::general_purpose::STANDARD.encode(read.data),
        cut: read.cut,
        skipped: read.skipped,
    })
}

/// Whether `name` has a session ID's shape (a UUID): only such a name is passed to the harness plugins for a session
/// this node no longer lists, never a caller's free text.
pub fn is_session_id(name: &str) -> bool {
    let parts: Vec<&str> = name.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A transcript here or on another node, for a caller on this node (`<name>` for a session, `plugin:<name>`).
pub async fn read(
    node: &Node,
    to: &str,
    range: TranscriptRange,
    caller: String,
    why: Option<String>,
) -> Result<TranscriptChunk, RpcError> {
    let (name, target) = crate::deliver::parse_address(to);
    match target {
        None => read_here(node, name, &range).await,
        Some(n) if n == node.name => read_here(node, name, &range).await,
        Some(n) => {
            let peer = node
                .link
                .peer()
                .await
                .ok_or_else(|| RpcError::not_here(format!("{} isn't linked to a control plane now", node.name)))?;
            let req = l::TranscriptRead { node: n.into(), session: name.into(), range, caller, justification: why };
            peer.request_within(l::method::TRANSCRIPT_READ, &req, Duration::from_secs(45)).await
        }
    }
}

/// A line up to this long is read whole (and can keep its JSON shape when cut); a longer one is read past, keeping
/// only its head.
const LINE_MEM_MAX: usize = 64 << 20;
/// How much of a line too long to read whole is kept, to find its type and show its start.
const HEAD_KEEP: usize = 64 * 1024;
/// How much of a cut line's start the marker shows when its JSON can't be kept.
const HEAD_SHOWN: usize = 2048;
/// The most a forward read looks through before it returns, however many lines `types` passes over: `next` says
/// where to carry on.
const SCAN_MAX: u64 = 64 << 20;

/// What [`read_range`] found: whole lines from `offset`, `next` where the following read starts.
#[derive(Debug)]
pub struct Lines {
    pub offset: u64,
    pub data: Vec<u8>,
    pub next: u64,
    pub size: u64,
    pub cut: u64,
    pub skipped: u64,
}

/// Read `range` of the file. Only whole lines come back (up to the last newline), so a follower never gets half a line
/// that's still being written. `clean` sees each line whole (redaction) before it's cut to fit. A forward read
/// (`offset`, or neither) returns the first lines that fit in `max`; a tail returns the newest, starting at the first
/// line that begins inside it (or the last whole line, when none does). A line longer than `line_max`, or than `max`,
/// is cut with a marker, never dropped, and `next` always lands on a line's start in the file.
pub fn read_range(
    path: &Path,
    range: &TranscriptRange,
    clean: &mut dyn FnMut(&[u8]) -> Vec<u8>,
) -> std::io::Result<Lines> {
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let max = range.max.unwrap_or(TRANSCRIPT_CHUNK_MAX).clamp(1, TRANSCRIPT_CHUNK_MAX) as usize;
    let cap = match range.line_max {
        Some(n) if n > 0 => (n as usize).min(max),
        _ => max,
    };
    let types = range.types.as_ref().filter(|t| !t.is_empty());
    let end = range.before.unwrap_or(size).min(size);
    // Where the whole lines end: just after the last newline before `end`.
    let stop = last_newline_before(&mut f, end)?.map_or(0, |i| i + 1);
    let tail = match (range.offset, range.tail) {
        (Some(_), _) | (None, None) => None,
        (None, Some(t)) => Some(t.min(TRANSCRIPT_TAIL_MAX)),
    };
    let start = match (range.offset, tail) {
        (Some(o), _) => o.min(size),
        (None, None) => 0,
        (None, Some(t)) => {
            let from = end.saturating_sub(t);
            match first_line_start(&mut f, from, stop)? {
                Some(p) => p,
                // No whole line begins inside the tail: the last whole line, however long.
                None => last_newline_before(&mut f, stop.saturating_sub(1))?.map_or(0, |i| i + 1),
            }
        }
    };
    let mut out = Lines { offset: start, data: vec![], next: start, size, cut: 0, skipped: 0 };
    if start >= stop {
        return Ok(out);
    }
    f.seek(SeekFrom::Start(start))?;
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, f);
    let wanted = |line: &RawLine| types.is_none_or(|t| line.kind().is_some_and(|k| t.contains(&k)));
    let mut pos = start;
    if tail.is_none() {
        // Forward: the first lines that fit.
        while pos < stop {
            let line = read_line(&mut reader, true)?;
            let next = pos + line.len + 1;
            if !wanted(&line) {
                out.skipped += 1;
            } else {
                let (bytes, was_cut) = line.emit(cap, clean);
                if out.data.len() + bytes.len() > max && !out.data.is_empty() {
                    break;
                }
                out.data.extend_from_slice(&bytes);
                out.cut += was_cut as u64;
            }
            pos = next;
            if pos - start >= SCAN_MAX {
                break;
            }
        }
        out.next = pos;
        return Ok(out);
    }
    // A tail: find the lines it holds, then send the newest that fit.
    let mut found: Vec<(u64, u64)> = vec![];
    while pos < stop {
        let line = read_line(&mut reader, types.is_some())?;
        if wanted(&line) {
            found.push((pos, out.skipped));
        } else {
            out.skipped += 1;
        }
        pos += line.len + 1;
    }
    out.next = pos;
    let all_skipped = out.skipped;
    let mut chosen: Vec<Vec<u8>> = vec![];
    let mut total = 0;
    for &(at, skipped_before) in found.iter().rev() {
        reader.seek(SeekFrom::Start(at))?;
        let (bytes, was_cut) = read_line(&mut reader, true)?.emit(cap, clean);
        if total + bytes.len() > max && !chosen.is_empty() {
            break;
        }
        total += bytes.len();
        out.cut += was_cut as u64;
        (out.offset, out.skipped) = (at, all_skipped - skipped_before);
        chosen.push(bytes);
    }
    if chosen.is_empty() {
        out.offset = start;
    }
    out.data.reserve(total);
    for bytes in chosen.iter().rev() {
        out.data.extend_from_slice(bytes);
    }
    Ok(out)
}

/// The position of the last newline before `end`, looking backwards from it.
fn last_newline_before(f: &mut std::fs::File, end: u64) -> std::io::Result<Option<u64>> {
    let mut hi = end;
    let mut buf = vec![0u8; 64 * 1024];
    while hi > 0 {
        let lo = hi.saturating_sub(buf.len() as u64);
        let n = (hi - lo) as usize;
        f.seek(SeekFrom::Start(lo))?;
        f.read_exact(&mut buf[..n])?;
        if let Some(i) = buf[..n].iter().rposition(|b| *b == b'\n') {
            return Ok(Some(lo + i as u64));
        }
        hi = lo;
    }
    Ok(None)
}

/// Where the first line that begins at or after `from` begins, if it's before `stop`.
fn first_line_start(f: &mut std::fs::File, from: u64, stop: u64) -> std::io::Result<Option<u64>> {
    if from == 0 {
        return Ok((stop > 0).then_some(0));
    }
    // Look from the byte before, so a tail that starts on a line keeps it.
    let mut at = from - 1;
    let mut buf = vec![0u8; 64 * 1024];
    while at < stop {
        f.seek(SeekFrom::Start(at))?;
        let want = buf.len().min((stop - at) as usize);
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        if let Some(i) = buf[..n].iter().position(|b| *b == b'\n') {
            let p = at + i as u64 + 1;
            return Ok((p < stop).then_some(p));
        }
        at += n as u64;
    }
    Ok(None)
}

/// One line as read: all of it when it's at most [`LINE_MEM_MAX`], else its first [`HEAD_KEEP`] bytes.
struct RawLine {
    /// Its length in the file, without the newline.
    len: u64,
    bytes: Vec<u8>,
    whole: bool,
}

/// The line the reader is at, which the caller knows ends with a newline; with `keep` false, only its length.
fn read_line(r: &mut impl std::io::BufRead, keep: bool) -> std::io::Result<RawLine> {
    let mut line = RawLine { len: 0, bytes: vec![], whole: keep };
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the transcript got shorter"));
        }
        let (part, done) = match buf.iter().position(|b| *b == b'\n') {
            Some(i) => (&buf[..i], true),
            None => (buf, false),
        };
        let n = part.len();
        if keep && line.whole && line.bytes.len() + n > LINE_MEM_MAX {
            line.whole = false;
            line.bytes.truncate(HEAD_KEEP);
            line.bytes.shrink_to_fit();
        }
        let room = match (keep, line.whole) {
            (false, _) => 0,
            (true, true) => n,
            (true, false) => HEAD_KEEP.saturating_sub(line.bytes.len()).min(n),
        };
        line.bytes.extend_from_slice(&part[..room]);
        line.len += n as u64;
        r.consume(n + done as usize);
        if done {
            return Ok(line);
        }
    }
}

impl RawLine {
    /// Its kind for `types`: the top-level `type`, or `tool_result` for a user line carrying tool results.
    fn kind(&self) -> Option<String> {
        if self.whole {
            #[derive(serde::Deserialize)]
            struct Line {
                #[serde(rename = "type")]
                kind: Option<String>,
                message: Option<Message>,
            }
            #[derive(serde::Deserialize)]
            struct Message {
                content: Option<Value>,
            }
            let line: Line = serde_json::from_slice(&self.bytes).ok()?;
            let kind = line.kind?;
            let content = line.message.and_then(|m| m.content);
            let results = content.as_ref().and_then(Value::as_array).is_some_and(|items| {
                items.iter().any(|i| i.get("type").and_then(Value::as_str) == Some("tool_result"))
            });
            Some(if kind == "user" && results { "tool_result".into() } else { kind })
        } else {
            let kind = top_level_type(&self.bytes)?;
            let results = self.bytes.windows(13).any(|w| w == b"\"tool_result\"");
            Some(if kind == "user" && results { "tool_result".into() } else { kind })
        }
    }

    /// The line to send, with its newline, and whether it was cut to fit in `cap`.
    fn emit(&self, cap: usize, clean: &mut dyn FnMut(&[u8]) -> Vec<u8>) -> (Vec<u8>, bool) {
        let clean_line = clean(&self.bytes);
        if self.whole && clean_line.len() < cap {
            let mut b = clean_line;
            b.push(b'\n');
            return (b, false);
        }
        if self.whole
            && let Ok(v) = serde_json::from_slice::<Value>(&clean_line)
            && let Some(mut b) = shrink(v, cap.saturating_sub(1), self.len)
        {
            b.push(b'\n');
            return (b, true);
        }
        let kind = if self.whole {
            serde_json::from_slice::<Value>(&self.bytes).ok().and_then(|v| v["type"].as_str().map(str::to_string))
        } else {
            top_level_type(&self.bytes)
        };
        let mut b = marker(kind, self.len, &clean_line, cap.saturating_sub(1));
        b.push(b'\n');
        (b, true)
    }
}

/// The line's JSON with its longest strings cut short so it serializes to at most `cap` bytes, with the cut key at
/// its root; `None` when it isn't an object or can't be made small enough that way.
fn shrink(mut v: Value, cap: usize, len: u64) -> Option<Vec<u8>> {
    v.as_object()?;
    let room = cap.checked_sub(64)?;
    // First every string to at most the room left, so the search below works on something small.
    cut_strings(&mut v, room);
    let fits = |limit: usize| {
        let mut c = v.clone();
        cut_strings(&mut c, limit);
        let b = serde_json::to_vec(&c).unwrap_or_default();
        (b.len() <= room).then_some(b)
    };
    if let Some(b) = fits(room) {
        return Some(with_cut_key(b, len));
    }
    let (mut lo, mut hi) = (32usize, room);
    let mut best = fits(lo)?;
    while lo + 1 < hi {
        let mid = (lo + hi) / 2;
        match fits(mid) {
            Some(b) => (lo, best) = (mid, b),
            None => hi = mid,
        }
    }
    Some(with_cut_key(best, len))
}

/// Add `"_fleetlayerCut":{"bytes":…}` to the end of a serialized object.
fn with_cut_key(mut b: Vec<u8>, len: u64) -> Vec<u8> {
    let key = format!("\"{}\":{{\"bytes\":{len}}}", l::TRANSCRIPT_CUT_KEY);
    b.pop(); // the closing brace
    if b.len() > 1 {
        b.push(b',');
    }
    b.extend_from_slice(key.as_bytes());
    b.push(b'}');
    b
}

/// Cut every string longer than `limit` bytes to `limit`, ending it with how long it was.
fn cut_strings(v: &mut Value, limit: usize) {
    match v {
        Value::String(s) => {
            // A string cut before keeps the mark with its first length.
            let earlier = s.rfind("…[cut: ").filter(|_| s.ends_with(']'));
            let mark = match earlier {
                Some(i) => s.split_off(i),
                None => format!("…[cut: {}]", human(s.len() as u64)),
            };
            if earlier.is_some() || s.len() > limit + mark.len() {
                let mut at = limit.min(s.len());
                while !s.is_char_boundary(at) {
                    at -= 1;
                }
                s.truncate(at);
                s.push_str(&mark);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| cut_strings(i, limit)),
        Value::Object(map) => map.values_mut().for_each(|i| cut_strings(i, limit)),
        _ => {}
    }
}

/// A line that can't keep its JSON shape: its type, its length and its first bytes, in at most `cap` bytes when the
/// type itself fits.
fn marker(kind: Option<String>, len: u64, head: &[u8], cap: usize) -> Vec<u8> {
    let text = String::from_utf8_lossy(head);
    let mut shown = text.len().min(HEAD_SHOWN);
    loop {
        while !text.is_char_boundary(shown) {
            shown -= 1;
        }
        let mut head = text[..shown].to_string();
        if shown < text.len() || (len as usize) > text.len() {
            head.push_str(&format!("…[cut: {}]", human(len)));
        }
        let mut obj = serde_json::Map::new();
        if let Some(k) = &kind {
            obj.insert("type".into(), Value::String(k.clone()));
        }
        obj.insert(l::TRANSCRIPT_CUT_KEY.into(), json!({"bytes": len, "head": head}));
        let b = serde_json::to_vec(&Value::Object(obj)).unwrap_or_default();
        if b.len() <= cap || shown == 0 {
            return b;
        }
        shown /= 2;
    }
}

/// The value of the top-level `"type"` key in the start of a JSON object, if it's there.
fn top_level_type(b: &[u8]) -> Option<String> {
    top_level_str(b, b"type")
}

/// The string value of a top-level key in the start of a JSON object, if it's there.
fn top_level_str(b: &[u8], key: &[u8]) -> Option<String> {
    let (mut depth, mut i) = (0i32, 0usize);
    // The end of the string starting at `i` (its opening quote), past escapes.
    let string_end = |i: usize| {
        let mut j = i + 1;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'"' => return Some(j),
                _ => j += 1,
            }
        }
        None
    };
    while i < b.len() {
        match b[i] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth -= 1,
            b'"' => {
                let e = string_end(i)?;
                if depth == 1 && &b[i + 1..e] == key {
                    let mut j = e + 1;
                    while j < b.len() && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if b.get(j) == Some(&b':') {
                        j += 1;
                        while j < b.len() && b[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if b.get(j) == Some(&b'"') {
                            let ve = string_end(j)?;
                            return serde_json::from_slice::<String>(&b[j..=ve]).ok();
                        }
                    }
                }
                i = e;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// A size for people: `512 bytes`, `820 KB`, `1.4 MB`.
fn human(n: u64) -> String {
    match n {
        0..1000 => format!("{n} bytes"),
        1000..1_000_000 => format!("{} KB", n / 1000),
        1_000_000..1_000_000_000 => format!("{:.1} MB", n as f64 / 1e6),
        _ => format!("{:.1} GB", n as f64 / 1e9),
    }
}

/// The shortest text a search looks for.
const SEARCH_MIN: usize = 3;
/// A search text sharing this many characters in a row with a delivered secret finds nothing (see
/// `Scrubber::overlaps`).
const SECRET_OVERLAP: usize = 6;
/// How long one node's search may take before it answers with what it found (`partial`).
const SEARCH_TIME: std::time::Duration = std::time::Duration::from_secs(25);
/// How much text an excerpt shows on each side of the hit.
const EXCERPT_SIDE: usize = 160;
/// How much of a transcript is read at once while searching: lines are never read whole.
const SEARCH_CHUNK: usize = 1 << 20;
/// The most excerpt text one answer carries.
const SEARCH_BYTES: usize = 256 * 1024;
/// How many searches this node runs in a minute, whoever asks (each reads its transcripts).
const SEARCHES_PER_MINUTE: usize = 30;

/// One search at a time on this node, and at most [`SEARCHES_PER_MINUTE`].
fn search_slot() -> &'static tokio::sync::Mutex<std::collections::VecDeque<std::time::Instant>> {
    static S: std::sync::OnceLock<tokio::sync::Mutex<std::collections::VecDeque<std::time::Instant>>> =
        std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// Search every transcript the harness plugins here know (their sessions' and their subagents') for `q.text`, newest
/// first, with each excerpt redacted.
pub async fn search_here(node: &Node, q: &l::TranscriptQuery) -> Result<l::TranscriptHits, RpcError> {
    let text = q.text.trim().to_string();
    if text.len() < SEARCH_MIN {
        return Err(RpcError::invalid_params(format!("search for at least {SEARCH_MIN} characters")));
    }
    let mut recent = tokio::time::timeout(Duration::from_secs(30), search_slot().lock())
        .await
        .map_err(|_| RpcError::refused(format!("{} is busy with another transcript search: try again", node.name)))?;
    let now = std::time::Instant::now();
    while recent.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
        recent.pop_front();
    }
    if recent.len() >= SEARCHES_PER_MINUTE {
        return Err(RpcError::refused(format!(
            "{} ran {SEARCHES_PER_MINUTE} transcript searches in the last minute: wait a little",
            node.name
        )));
    }
    recent.push_back(now);
    let mut files: Vec<ext::TranscriptFile> = vec![];
    for p in &node.plugins {
        if !p.roles().await.harness.is_some_and(|h| h.list) {
            continue;
        }
        let Some(peer) = p.peer().await else { continue };
        match peer
            .request_within::<_, Value>(ext::method::HARNESS_TRANSCRIPTS, &json!({}), Duration::from_secs(20))
            .await
        {
            Ok(v) => files.extend(
                serde_json::from_value::<Vec<ext::TranscriptFile>>(v["transcripts"].clone()).unwrap_or_default(),
            ),
            Err(e) => tracing::warn!(plugin = %p.name(), "listing transcripts: {}", e.message),
        }
    }
    let scrubber = node.secrets.scrubber();
    // A text that shares a piece of a delivered secret finds nothing, answered just as a search that found nothing:
    // hit or miss would say whether that piece is in the transcripts.
    if scrubber.overlaps(&text, SECRET_OVERLAP) {
        tracing::warn!("a transcript search for a piece of a delivered secret was answered with nothing");
        return Ok(l::TranscriptHits { node: node.name.clone(), searched: files.len() as u64, ..Default::default() });
    }
    let tabs: std::collections::HashMap<String, String> = node
        .registry
        .all()
        .await
        .into_iter()
        .filter_map(|s| Some((s.session_id.clone(), s.tab()?.to_string())))
        .collect();
    let limit = q.limit.unwrap_or(50).clamp(1, l::TRANSCRIPT_SEARCH_MAX) as usize;
    let per = q.per_session.unwrap_or(3).clamp(1, 100) as usize;
    let deadline = std::time::Instant::now() + SEARCH_TIME;
    let (mut found, hits) = tokio::task::spawn_blocking(move || {
        let mut hits = vec![];
        let found = search_files(files, &text, limit, per, deadline, &mut |window: &str| {
            let (out, h) = scrubber.scrub(window);
            hits.extend(h);
            out.into_owned()
        });
        (found, hits)
    })
    .await
    .map_err(|e| RpcError::internal(e.to_string()))?;
    drop(recent);
    node.secrets.report(&hits, &format!("a transcript search on {}", node.name));
    for h in &mut found.hits {
        h.tab = tabs.get(&h.session_id).cloned();
    }
    found.node = node.name.clone();
    Ok(found)
}

/// A search here or on another node, for a caller on this node.
pub async fn search(
    node: &Node,
    on: Option<&str>,
    query: l::TranscriptQuery,
    caller: String,
) -> Result<l::TranscriptHits, RpcError> {
    match on {
        None => search_here(node, &query).await,
        Some(n) if n == node.name => search_here(node, &query).await,
        Some(n) => {
            let peer = node
                .link
                .peer()
                .await
                .ok_or_else(|| RpcError::not_here(format!("{} isn't linked to a control plane now", node.name)))?;
            let req = l::TranscriptSearch { node: n.into(), query, caller };
            peer.request_within(l::method::TRANSCRIPT_SEARCH, &req, Duration::from_secs(45)).await.map_err(|e| {
                if e.code == fleetlayer_protocol::rpc::code::METHOD_NOT_FOUND {
                    RpcError::refused("the control plane is too old to search transcripts on other nodes: update it")
                } else {
                    e
                }
            })
        }
    }
}

/// Look through `files`, newest first, for `text` whatever its case: at most one hit a line, `per` in one file and
/// `limit` in all, until `deadline` or [`SEARCH_BYTES`] of excerpts. `clean` redacts the text around a hit before the
/// excerpt is cut from it.
pub fn search_files(
    files: Vec<ext::TranscriptFile>,
    text: &str,
    limit: usize,
    per: usize,
    deadline: std::time::Instant,
    clean: &mut dyn FnMut(&str) -> String,
) -> l::TranscriptHits {
    search_files_in(files, text, limit, per, deadline, SEARCH_CHUNK, clean)
}

fn search_files_in(
    mut files: Vec<ext::TranscriptFile>,
    text: &str,
    limit: usize,
    per: usize,
    deadline: std::time::Instant,
    chunk: usize,
    clean: &mut dyn FnMut(&str) -> String,
) -> l::TranscriptHits {
    let mut out = l::TranscriptHits::default();
    let Ok(finder) = aho_corasick::AhoCorasick::builder().ascii_case_insensitive(true).build([text]) else {
        return out;
    };
    let modified = |f: &ext::TranscriptFile| std::fs::metadata(&f.path).and_then(|m| m.modified()).ok();
    let mut dated: Vec<_> = files.drain(..).map(|f| (modified(&f), f)).collect();
    dated.sort_by_key(|a| std::cmp::Reverse(a.0));
    let total = dated.len();
    let mut bytes = 0usize;
    for (i, (_, f)) in dated.into_iter().enumerate() {
        if out.hits.len() >= limit || bytes >= SEARCH_BYTES || std::time::Instant::now() >= deadline {
            out.partial = i < total;
            break;
        }
        out.searched += 1;
        let Ok(found) = search_file(&f, &finder, text.len(), per.min(limit - out.hits.len()), deadline, chunk) else {
            continue;
        };
        if found.1 {
            out.partial = true;
        }
        for (offset, at) in found.0 {
            let Some(hit) = hit_at(&f, offset, at, text.len(), &finder, clean) else { continue };
            bytes += hit.excerpt.len();
            out.hits.push(hit);
            if bytes >= SEARCH_BYTES {
                out.partial = true;
                break;
            }
        }
    }
    out
}

/// One file read `chunk` bytes at a time: each hit as (where its line starts, where the text starts), at most one a
/// line and `per` in all, and whether the deadline stopped it.
fn search_file(
    f: &ext::TranscriptFile,
    finder: &aho_corasick::AhoCorasick,
    len: usize,
    per: usize,
    deadline: std::time::Instant,
    chunk: usize,
) -> std::io::Result<(Vec<(u64, u64)>, bool)> {
    let mut file = std::fs::File::open(&f.path)?;
    let carry_max = len.saturating_sub(1);
    let mut buf: Vec<u8> = Vec::with_capacity(chunk + carry_max);
    // `buf` starts this far into the file; the line being read started at `line`; after a hit, the rest of its line is
    // passed over.
    let (mut base, mut line, mut skipping) = (0u64, 0u64, false);
    let mut hits = vec![];
    let mut piece = vec![0u8; chunk];
    loop {
        if std::time::Instant::now() >= deadline {
            return Ok((hits, true));
        }
        let n = file.read(&mut piece)?;
        if n == 0 {
            return Ok((hits, false));
        }
        let carried = buf.len();
        buf.extend_from_slice(&piece[..n]);
        // The new hits (one may begin in the carried bytes) and the new newlines, in order. A hit has no newline in
        // it, so one that began in the carried bytes is on the line the last piece ended in.
        let mut events: Vec<(usize, bool)> = finder
            .find_iter(&buf[..])
            .filter(|m| m.end() > carried)
            .map(|m| (m.start(), true))
            .chain(buf[carried..].iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| (carried + i, false)))
            .collect();
        events.sort_unstable();
        for (pos, is_hit) in events {
            if !is_hit {
                line = base + pos as u64 + 1;
                skipping = false;
            } else if !skipping {
                hits.push((line, base + pos as u64));
                skipping = true;
                if hits.len() >= per {
                    return Ok((hits, false));
                }
            }
        }
        // Keep the last few bytes, so a text across two pieces is found.
        let keep = carry_max.min(buf.len());
        base += (buf.len() - keep) as u64;
        buf.drain(..buf.len() - keep);
    }
}

/// The hit at `at` (its line starting at `line`): the line's type and time from its start, and an excerpt cut from a
/// window around it, redacted first.
fn hit_at(
    f: &ext::TranscriptFile,
    line: u64,
    at: u64,
    len: usize,
    finder: &aho_corasick::AhoCorasick,
    clean: &mut dyn FnMut(&str) -> String,
) -> Option<l::TranscriptHit> {
    const WIDE: u64 = 8192;
    let mut file = std::fs::File::open(&f.path).ok()?;
    let read_at = |file: &mut std::fs::File, from: u64, n: usize| -> Option<Vec<u8>> {
        file.seek(SeekFrom::Start(from)).ok()?;
        let mut b = Vec::with_capacity(n);
        file.take(n as u64).read_to_end(&mut b).ok()?;
        Some(b)
    };
    let head = read_at(&mut file, line, HEAD_KEEP)?;
    let head = &head[..head.iter().position(|b| *b == b'\n').unwrap_or(head.len())];
    let lo = at.saturating_sub(WIDE).max(line);
    let mut window = read_at(&mut file, lo, (at - lo + len as u64 + WIDE) as usize)?;
    // Only this line: up to its newline after the hit.
    let hit_in = (at - lo) as usize;
    if let Some(i) = window[hit_in..].iter().position(|b| *b == b'\n') {
        window.truncate(hit_in + i);
    }
    Some(l::TranscriptHit {
        session_id: f.session_id.clone(),
        agent_id: f.agent_id.clone(),
        tab: None,
        offset: line,
        kind: top_level_type(head),
        timestamp: top_level_str(head, b"timestamp"),
        excerpt: excerpt(&window, hit_in, lo > line, finder, clean),
    })
}

/// The text around a hit in `window`: all of it redacted first, then cut to [`EXCERPT_SIDE`] on each side of the text
/// found again in it (not found: the text itself was redacted, and the window's middle is shown).
fn excerpt(
    window: &[u8],
    start: usize,
    more_before: bool,
    finder: &aho_corasick::AhoCorasick,
    clean: &mut dyn FnMut(&str) -> String,
) -> String {
    let text = clean(&String::from_utf8_lossy(window));
    let near = start.min(text.len());
    let (s, e) = match finder.find_iter(text.as_bytes()).min_by_key(|m| m.start().abs_diff(near)) {
        Some(m) => (m.start(), m.end()),
        None => (near, near),
    };
    let mut a = s.saturating_sub(EXCERPT_SIDE);
    while !text.is_char_boundary(a) {
        a -= 1;
    }
    let mut b = (e + EXCERPT_SIDE).min(text.len());
    while !text.is_char_boundary(b) {
        b += 1;
    }
    let mut out = String::new();
    if a > 0 || more_before {
        out.push('…');
    }
    out.push_str(&text[a..b].replace("\\n", " "));
    if b < text.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, text).unwrap();
        (dir, p)
    }

    fn r(offset: Option<u64>, tail: Option<u64>, max: Option<u64>) -> TranscriptRange {
        TranscriptRange { offset, tail, max, ..Default::default() }
    }

    fn read(p: &Path, range: &TranscriptRange) -> Lines {
        read_range(p, range, &mut |b: &[u8]| b.to_vec()).unwrap()
    }

    fn text(l: &Lines) -> &str {
        std::str::from_utf8(&l.data).unwrap()
    }

    fn values(l: &Lines) -> Vec<Value> {
        text(l).lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }

    /// A Claude Code line of `kind` whose text is `body`.
    fn line(kind: &str, body: &str) -> String {
        let content = if kind == "tool_result" {
            json!([{"type": "tool_result", "tool_use_id": "t1", "content": body}])
        } else {
            json!([{"type": "text", "text": body}])
        };
        let kind = if kind == "tool_result" { "user" } else { kind };
        format!("{}\n", json!({"type": kind, "uuid": "u", "message": {"role": kind, "content": content}}))
    }

    #[test]
    fn only_whole_lines_come_back() {
        let (_d, p) = file("{\"a\":1}\n{\"b\":2}\n{\"c\":");
        let l = read(&p, &r(None, None, None));
        assert_eq!((l.offset, text(&l), l.next, l.size), (0, "{\"a\":1}\n{\"b\":2}\n", 16, 21));
        // Following on from where the last read stopped: the half-written line waits.
        let l = read(&p, &r(Some(16), None, None));
        assert_eq!((l.offset, l.data.len(), l.next), (16, 0, 16));
        let l = read(&p, &r(Some(0), None, Some(10)));
        assert_eq!((text(&l), l.next), ("{\"a\":1}\n", 8), "a limit cuts at the last whole line");
    }

    #[test]
    fn a_tail_starts_at_a_line() {
        let (_d, p) = file("line one\nline two\nline three\n");
        let l = read(&p, &r(None, Some(14), None));
        assert_eq!((l.offset, text(&l)), (18, "line three\n"), "the line that begins inside the tail");
        let l = read(&p, &r(None, Some(20), None));
        assert_eq!((l.offset, text(&l)), (9, "line two\nline three\n"), "a tail that starts on a line keeps it");
        assert_eq!(read(&p, &r(None, Some(1000), None)).offset, 0);
    }

    /// The overseer's case: a transcript whose last lines are each longer than the tail and than a read. Each comes
    /// back cut, still JSON in Claude Code's shape, never as nothing.
    #[test]
    fn a_tail_shorter_than_the_last_line_still_returns_it_cut() {
        let big = "x".repeat(3 << 20);
        let (_d, p) =
            file(&format!("{}{}{}", line("user", "hello"), line("tool_result", &big), line("tool_result", &big)));
        let l = read(&p, &r(None, Some(1000), None));
        assert_eq!(l.cut, 1);
        let v = values(&l);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["type"], "user");
        let cut = v[0]["message"]["content"][0]["content"].as_str().unwrap();
        assert!(cut.ends_with("…[cut: 3.1 MB]") && cut.len() < TRANSCRIPT_CHUNK_MAX as usize, "{}", cut.len());
        assert!(v[0][l::TRANSCRIPT_CUT_KEY]["bytes"].as_u64().unwrap() > 3 << 20);
        assert_eq!(l.next, l.size);
        assert!(l.data.len() <= TRANSCRIPT_CHUNK_MAX as usize);
    }

    /// Following a transcript page by page across over-long lines: every line comes once, in order, and `next` always
    /// lands on the start of a line in the file.
    #[test]
    fn pages_stay_on_lines_across_cut_lines() {
        let mut all = String::new();
        for i in 0..6 {
            let body = if i % 2 == 1 { "y".repeat(50_000) } else { format!("turn {i}") };
            all.push_str(&line("assistant", &body));
        }
        let (_d, p) = file(&all);
        let mut range =
            TranscriptRange { offset: Some(0), max: Some(30_000), line_max: Some(4096), ..Default::default() };
        let mut got = vec![];
        loop {
            let l = read(&p, &range);
            if l.data.is_empty() {
                assert_eq!(l.next, l.size);
                break;
            }
            assert!(l.next == l.size || all.as_bytes()[l.next as usize - 1] == b'\n');
            got.extend(values(&l));
            range.offset = Some(l.next);
        }
        assert_eq!(got.len(), 6);
        for (i, v) in got.iter().enumerate() {
            let t = v["message"]["content"][0]["text"].as_str().unwrap();
            if i % 2 == 1 {
                assert!(t.len() < 4096 && t.ends_with("…[cut: 50 KB]"), "{i}: {}", t.len());
            } else {
                assert_eq!(t, format!("turn {i}"));
                assert!(v.get(l::TRANSCRIPT_CUT_KEY).is_none());
            }
        }
    }

    /// `types` keeps the conversation and passes over tool results, so a tail of a transcript full of them still
    /// brings back the newest turns; the newest are the ones kept when they don't all fit.
    #[test]
    fn types_keep_the_newest_turns() {
        let mut all = String::new();
        for i in 0..20 {
            all.push_str(&line("user", &format!("question {i}")));
            all.push_str(&line("tool_result", &"z".repeat(100_000)));
            all.push_str(&line("assistant", &format!("answer {i}")));
        }
        let (_d, p) = file(&all);
        let types = Some(vec!["user".to_string(), "assistant".to_string()]);
        let range = TranscriptRange { tail: Some(10 << 20), max: Some(1000), types, ..Default::default() };
        let l = read(&p, &range);
        let v = values(&l);
        assert!(v.len() >= 4, "{}", v.len());
        assert_eq!(v.last().unwrap()["message"]["content"][0]["text"], "answer 19");
        assert!(v.iter().all(|v| v["message"]["content"][0]["type"] == "text"));
        assert_eq!(l.next, l.size);
        // The tool results between the first line sent and the end were passed over.
        assert_eq!(l.skipped as usize, v.len() / 2);
        // A forward read with the same filter.
        let range = TranscriptRange { offset: Some(0), types: range.types.clone(), ..Default::default() };
        let l = read(&p, &range);
        assert_eq!((values(&l).len(), l.skipped, l.offset, l.next), (40, 20, 0, l.size));
    }

    /// A line too long to read whole becomes a marker with its type and its start.
    #[test]
    fn a_line_too_long_to_read_whole_becomes_a_marker() {
        let big = format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{}\"}}}}\n", "q".repeat(LINE_MEM_MAX + 10));
        let (_d, p) = file(&format!("{}{big}", line("assistant", "hi")));
        let l = read(&p, &r(None, Some(10), None));
        let v = values(&l);
        assert_eq!((v.len(), l.cut), (1, 1));
        assert_eq!(v[0]["type"], "user");
        let cut = &v[0][l::TRANSCRIPT_CUT_KEY];
        assert_eq!(cut["bytes"].as_u64().unwrap() as usize, big.len() - 1);
        assert!(cut["head"].as_str().unwrap().starts_with("{\"type\":\"user\""));
        assert!(cut["head"].as_str().unwrap().ends_with("…[cut: 67.1 MB]"));
    }

    #[test]
    fn before_ends_a_read_for_paging_backwards() {
        let (_d, p) = file("one\ntwo\nthree\nfour\n");
        let range = TranscriptRange { tail: Some(10), before: Some(14), ..Default::default() };
        let l = read(&p, &range);
        assert_eq!((text(&l), l.offset, l.next), ("two\nthree\n", 4, 14));
    }

    fn tfile(dir: &Path, session: &str, agent: Option<&str>, text: &str) -> ext::TranscriptFile {
        let p = dir.join(format!("{session}-{}.jsonl", agent.unwrap_or("main")));
        std::fs::write(&p, text).unwrap();
        ext::TranscriptFile {
            session_id: session.into(),
            agent_id: agent.map(str::to_string),
            path: p.to_string_lossy().into_owned(),
            format: "claude-jsonl".into(),
        }
    }

    fn later() -> std::time::Instant {
        std::time::Instant::now() + std::time::Duration::from_secs(30)
    }

    /// "Who has this ID?": every transcript's lines holding the text, whatever its case, with where each starts, its
    /// type and time, and the text around it; at most `per` hits in one transcript and `limit` in all.
    #[test]
    fn a_search_finds_the_text_in_every_transcript_whatever_its_case() {
        let dir = tempfile::tempdir().unwrap();
        let first =
            r#"{"type":"user","timestamp":"2026-11-28T05:00:00Z","message":{"content":"look at SC-4242 please"}}"#;
        let a = tfile(
            dir.path(),
            "s-a",
            None,
            &format!("{}\n{first}\n{}\n", line("assistant", "hi").trim(), line("assistant", "sc-4242 done").trim()),
        );
        let b = tfile(dir.path(), "s-b", Some("a1b2"), &line("assistant", "nothing here"));
        let c = tfile(
            dir.path(),
            "s-c",
            Some("c3d4"),
            &line("assistant", &format!("{}Sc-4242{}", "x".repeat(1000), "y".repeat(1000))),
        );
        let found = search_files(vec![a, b, c], "sc-4242", 50, 3, later(), &mut |w: &str| w.to_string());
        assert_eq!((found.searched, found.partial), (3, false));
        assert_eq!(found.hits.len(), 3);
        let first_hit = found.hits.iter().find(|h| h.session_id == "s-a").unwrap();
        let hi = line("assistant", "hi");
        assert_eq!(first_hit.offset as usize, hi.len());
        assert_eq!(
            (first_hit.kind.as_deref(), first_hit.timestamp.as_deref()),
            (Some("user"), Some("2026-11-28T05:00:00Z"))
        );
        assert!(first_hit.excerpt.contains("look at SC-4242 please"), "{}", first_hit.excerpt);
        let sub = found.hits.iter().find(|h| h.session_id == "s-c").unwrap();
        assert_eq!(sub.agent_id.as_deref(), Some("c3d4"));
        assert!(
            sub.excerpt.starts_with('…') && sub.excerpt.ends_with('…') && sub.excerpt.len() < 400,
            "{}",
            sub.excerpt
        );
        assert!(sub.excerpt.contains("Sc-4242"));

        let a = tfile(dir.path(), "s-a", None, &line("assistant", "sc-4242").repeat(10));
        let found = search_files(vec![a.clone()], "SC-4242", 50, 3, later(), &mut |w: &str| w.to_string());
        assert_eq!(found.hits.len(), 3, "at most per_session in one transcript");
        let found = search_files(vec![a.clone(), a], "SC-4242", 2, 3, later(), &mut |w: &str| w.to_string());
        assert_eq!((found.hits.len(), found.partial), (2, true), "the limit stops it, with a transcript left");
    }

    /// Files are read a piece at a time, never a line whole: a text split across two pieces is still found, once a
    /// line, with the line's own start, even deep inside a line far longer than a piece.
    #[test]
    fn a_search_reads_pieces_and_finds_text_across_them_once_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let long = line("tool_result", &format!("{}ORDER-77881 and order-77881 again", "z".repeat(5000)));
        let short = line("assistant", "see order-77881");
        let f = tfile(dir.path(), "s", None, &format!("{long}{short}"));
        for chunk in [7, 16, 1000, 1 << 20] {
            let found =
                search_files_in(vec![f.clone()], "order-77881", 50, 10, later(), chunk, &mut |w: &str| w.to_string());
            let offsets: Vec<u64> = found.hits.iter().map(|h| h.offset).collect();
            assert_eq!(offsets, [0, long.len() as u64], "chunk {chunk}: one hit a line, at the line's start");
            assert_eq!(found.hits[0].kind.as_deref(), Some("user"));
            assert!(found.hits[0].excerpt.contains("ORDER-77881"), "{}", found.hits[0].excerpt);
            assert!(found.hits[0].excerpt.len() < 400);
        }
    }

    /// A text that shares six characters with a delivered secret finds nothing, answered as a search that found
    /// nothing, so hit or miss can't recover the secret a character at a time.
    #[test]
    fn a_search_for_a_piece_of_a_secret_finds_nothing() {
        let s = fleetlayer_secrets::Secret { reference: "op://v/i/f".into(), value: "tok_9f8e7d6c5b4a".into() };
        let scrubber = fleetlayer_secrets::scrub::Scrubber::new(std::slice::from_ref(&s));
        assert!(scrubber.overlaps("tok_9f", SECRET_OVERLAP));
        assert!(!scrubber.overlaps("order-77881", SECRET_OVERLAP));
    }

    /// The text around a hit is redacted before it's cut, so a secret beside an ID never shows.
    #[test]
    fn a_search_excerpt_is_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let f = tfile(dir.path(), "s", None, &line("assistant", "deploy key tok_9f8e7d6c for order-77881 today"));
        let found = search_files(vec![f], "order-77881", 50, 3, later(), &mut |w: &str| {
            w.replace("tok_9f8e7d6c", "[REDACTED x]")
        });
        let e = &found.hits[0].excerpt;
        assert!(e.contains("[REDACTED x] for order-77881") && !e.contains("9f8e"), "{e}");
    }

    #[test]
    fn the_top_level_type_is_found_in_a_head() {
        assert_eq!(top_level_type(br#"{"message":{"type":"x"},"type" : "assistant","#).as_deref(), Some("assistant"));
        assert_eq!(top_level_type(br#"{"a":"\"type\"","type":"user""#).as_deref(), Some("user"));
        assert_eq!(top_level_type(br#"{"message":{"type":"x","content":"#), None);
    }

    /// A delivered secret in a transcript line, as Claude Code writes one (inside a JSON string, so JSON-escaped), is
    /// redacted before the line is cut, and the copy still decodes.
    #[test]
    fn secrets_are_redacted_before_a_line_is_cut() {
        let home = tempfile::tempdir().unwrap();
        let s = fleetlayer_secrets::Secret {
            reference: "op://vault/item/token".into(),
            value: "tok_\"quoted\"_9f8e7d6c5b4a".into(),
        };
        fleetlayer_secrets::registry::register(home.path(), "1password", std::slice::from_ref(&s)).unwrap();
        let secrets = crate::secrets::Secrets::new(home.path().to_path_buf());
        let scrubber = secrets.scrubber();
        // The secret sits where a cut would split it.
        let body = format!("{}the token is {}{}", "a".repeat(2000), s.value, "b".repeat(5000));
        let (_d, p) = file(&line("assistant", &body));
        for line_max in [None, Some(2030)] {
            let range = TranscriptRange { line_max, ..Default::default() };
            let l = read_range(&p, &range, &mut |b: &[u8]| {
                scrubber.scrub(&String::from_utf8_lossy(b)).0.into_owned().into_bytes()
            })
            .unwrap();
            let out = text(&l);
            assert!(!out.contains("9f8e") && !out.contains("tok_"), "{out}");
            let v = values(&l);
            let t = v[0]["message"]["content"][0]["text"].as_str().unwrap();
            assert!(line_max.is_some() || t.contains("the token is [REDACTED op://vault/item/token]"), "{t}");
        }
    }
}
