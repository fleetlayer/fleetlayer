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
use fleetlayer_protocol::link::{self as l, TRANSCRIPT_CHUNK_MAX, TranscriptChunk, TranscriptRange};
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
    let (offset, bytes, size) = tokio::task::spawn_blocking(move || read_range(Path::new(&path), &range2))
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?
        .map_err(|e| RpcError::refused(format!("reading {name}'s transcript: {e}")))?;
    let next = offset + bytes.len() as u64;
    let clean = scrubbed(&node.secrets, &bytes, &format!("the transcript of {name} on {}", node.name));
    Ok(TranscriptChunk {
        session_id: session.session_id,
        format,
        size,
        offset,
        next,
        data: base64::engine::general_purpose::STANDARD.encode(clean),
    })
}

/// The lines with every secret delivered to this node redacted, in plain text, before they're encoded: the scrubber
/// on the link to the control plane sees only base64 by then, where a redaction would break the encoding and a value
/// JSON-escaped inside it wouldn't be found.
pub fn scrubbed(secrets: &crate::secrets::Secrets, bytes: &[u8], context: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    crate::secrets::scrub_with(secrets, &text, context).0.into_owned().into_bytes()
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

/// Read `range` of the file: `(offset, bytes, size)`. Only whole lines come back (up to the last newline), so a
/// follower never gets half a line that's still being written; a single line longer than the limit comes back whole
/// as far as the limit. A tail starts at the first line that begins inside it.
pub fn read_range(path: &Path, range: &TranscriptRange) -> std::io::Result<(u64, Vec<u8>, u64)> {
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let max = range.max.unwrap_or(TRANSCRIPT_CHUNK_MAX).clamp(1, TRANSCRIPT_CHUNK_MAX);
    let mut start = match (range.offset, range.tail) {
        (Some(o), _) => o.min(size),
        (None, Some(t)) => size.saturating_sub(t),
        (None, None) => 0,
    };
    if range.offset.is_none() && start > 0 {
        // Find where the next line begins: look from the byte before, so a tail that starts on a line keeps it.
        f.seek(SeekFrom::Start(start - 1))?;
        let mut probe = vec![0u8; 64 * 1024];
        let n = f.read(&mut probe)?;
        match probe[..n].iter().position(|b| *b == b'\n') {
            Some(i) => start = start - 1 + i as u64 + 1,
            None => return Ok((size, vec![], size)),
        }
    }
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity(max.min(size - start) as usize);
    f.take(max).read_to_end(&mut buf)?;
    match buf.iter().rposition(|b| *b == b'\n') {
        Some(i) => buf.truncate(i + 1),
        None if buf.len() as u64 == max => {}
        None => buf.clear(),
    }
    Ok((start, buf, size))
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
        TranscriptRange { offset, tail, max }
    }

    #[test]
    fn only_whole_lines_come_back() {
        let (_d, p) = file("{\"a\":1}\n{\"b\":2}\n{\"c\":");
        let (o, b, size) = read_range(&p, &r(None, None, None)).unwrap();
        assert_eq!((o, String::from_utf8(b).unwrap().as_str(), size), (0, "{\"a\":1}\n{\"b\":2}\n", 21));
        // Following on from where the last read stopped: the half-written line waits.
        let (o, b, _) = read_range(&p, &r(Some(16), None, None)).unwrap();
        assert_eq!((o, b.len()), (16, 0));
        let (_, b, _) = read_range(&p, &r(Some(0), None, Some(10))).unwrap();
        assert_eq!(b, b"{\"a\":1}\n", "a limit cuts at the last whole line");
    }

    #[test]
    fn a_tail_starts_at_a_line() {
        let (_d, p) = file("line one\nline two\nline three\n");
        let (o, b, _) = read_range(&p, &r(None, Some(14), None)).unwrap();
        assert_eq!((o, b.as_slice()), (18, &b"line three\n"[..]), "the line that begins inside the tail");
        let (o, b, _) = read_range(&p, &r(None, Some(20), None)).unwrap();
        assert_eq!((o, b.as_slice()), (9, &b"line two\nline three\n"[..]), "a tail that starts on a line keeps it");
        let (o, _, _) = read_range(&p, &r(None, Some(1000), None)).unwrap();
        assert_eq!(o, 0);
    }

    /// A delivered secret in a transcript line, as Claude Code writes one (inside a JSON string, so JSON-escaped), is
    /// redacted before encoding, and the copy still decodes.
    #[test]
    fn secrets_are_redacted_in_plain_text_before_encoding() {
        let home = tempfile::tempdir().unwrap();
        let s = fleetlayer_secrets::Secret {
            reference: "op://vault/item/token".into(),
            value: "tok_\"quoted\"_9f8e7d6c5b4a".into(),
        };
        fleetlayer_secrets::registry::register(home.path(), "1password", std::slice::from_ref(&s)).unwrap();
        let secrets = crate::secrets::Secrets::new(home.path().to_path_buf());
        let line = format!("{}\n", serde_json::json!({"message": {"content": format!("the token is {}", s.value)}}));
        let out = String::from_utf8(scrubbed(&secrets, line.as_bytes(), "a test")).unwrap();
        assert!(!out.contains("9f8e7d6c5b4a"), "{out}");
        assert!(out.contains("[REDACTED op://vault/item/token]"), "{out}");
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert!(v["message"]["content"].as_str().unwrap().starts_with("the token is [REDACTED"));
    }

    #[test]
    fn one_huge_line_comes_back_as_far_as_the_limit() {
        let (_d, p) = file(&"x".repeat(100));
        let (_, b, _) = read_range(&p, &r(Some(0), None, Some(40))).unwrap();
        assert_eq!(b.len(), 40);
    }
}
