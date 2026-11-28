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
    let session = crate::deliver::resolve(node, name).await?;
    let none = || RpcError::refused(format!("{name} on {} has no harness plugin that knows its transcript", node.name));
    let plugin_name = session.harness_plugin.clone().ok_or_else(none)?;
    let plugin = node.plugin(&plugin_name).ok_or_else(none)?;
    let peer = plugin.peer().await.ok_or_else(|| RpcError::refused(format!("plugin {plugin_name} isn't running")))?;
    let v: Value = peer
        .request_within(
            ext::method::HARNESS_TRANSCRIPT,
            &json!({"sessionId": session.session_id}),
            Duration::from_secs(10),
        )
        .await?;
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
        session_id: session.session_id,
        format,
        size: read.size,
        offset: read.offset,
        next: read.next,
        data: base64::engine::general_purpose::STANDARD.encode(read.data),
        cut: read.cut,
        skipped: read.skipped,
    })
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
                if depth == 1 && &b[i + 1..e] == b"type" {
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
