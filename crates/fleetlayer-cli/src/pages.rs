//! `fleetlayer pages push <dir> --space <space>`: send a page to a space hosted on another machine, through this
//! node and the control plane (`docs/plugins/pages.md`, "From another machine"). Only files that changed are sent.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The receiver's limits, checked here first for a clearer message.
pub const MAX_FILES: usize = 200;
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug)]
pub struct File {
    pub rel: String,
    pub full: PathBuf,
    pub size: u64,
    pub sha256: String,
}

/// The page's files: everything under `dir` except hidden (`.`) and private (`_`) names and `node_modules`, which
/// are never served. Symbolic links are skipped and listed, since the page on the other machine can't follow them.
pub fn collect(dir: &Path) -> anyhow::Result<(Vec<File>, Vec<String>)> {
    let mut files = vec![];
    let mut links = vec![];
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for e in std::fs::read_dir(dir.join(&rel))? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with('_') || name == "node_modules" {
                continue;
            }
            let r = rel.join(&name);
            let t = e.file_type()?;
            if t.is_symlink() {
                links.push(r.to_string_lossy().into_owned());
            } else if t.is_dir() {
                stack.push(r);
            } else if t.is_file() {
                let rel = r.to_str().ok_or_else(|| anyhow::anyhow!("{}: the name isn't UTF-8", r.display()))?;
                let full = dir.join(&r);
                files.push(File {
                    rel: rel.replace('\\', "/"),
                    size: e.metadata()?.len(),
                    sha256: sha256(&full)?,
                    full,
                });
            }
            anyhow::ensure!(files.len() <= MAX_FILES, "the page has more than {MAX_FILES} files");
        }
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    anyhow::ensure!(!files.is_empty(), "{} has no files to push", dir.display());
    anyhow::ensure!(
        files.iter().any(|f| f.rel == "index.md" || f.rel == "index.html"),
        "{} has no index.md (or index.html for a mock-up)",
        dir.display()
    );
    let total: u64 = files.iter().map(|f| f.size).sum();
    anyhow::ensure!(
        total <= MAX_BYTES,
        "the page is {} MB; the limit is {} MB",
        total / 1_000_000,
        MAX_BYTES / 1_000_000
    );
    Ok((files, links))
}

fn sha256(p: &Path) -> anyhow::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// The page's name from its folder: `2026-11-27-signing-qa` → `signing-qa`.
pub fn default_name(dir: &Path) -> Option<String> {
    let base = dir.canonicalize().ok()?.file_name()?.to_str()?.to_string();
    let b = base.as_bytes();
    let dated = b.len() > 11
        && b[..10].iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
        && b[10] == b'-';
    Some(if dated { base[11..].to_string() } else { base })
}

pub struct Push<'a> {
    pub dir: &'a Path,
    pub space: &'a str,
    pub name: Option<&'a str>,
    pub new: bool,
    pub why: Option<&'a str>,
    pub caller: Value,
}

/// Pushes the page; prints its link and any problems the pages service found.
pub async fn push(socket: &Path, p: Push<'_>) -> anyhow::Result<()> {
    let name = match p.name {
        Some(n) => n.to_string(),
        None => default_name(p.dir).ok_or_else(|| anyhow::anyhow!("give the page a name with --name"))?,
    };
    anyhow::ensure!(
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "page name {name:?}: use lower-case letters, digits and hyphens (--name)"
    );
    let (files, links) = collect(p.dir)?;
    for l in &links {
        eprintln!("skipping {l}: a symbolic link");
    }
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| anyhow::anyhow!("can't reach the node's tools socket {} ({e})", socket.display()))?;
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let date = jiff::ZonedDateTime::now().date().to_string();
    let req = json!({
        "pages": p.space, "name": name, "new": p.new, "date": date, "why": p.why,
        "sessionId": p.caller["sessionId"], "address": p.caller["address"],
    });
    w.write_all(format!("{req}\n").as_bytes()).await?;
    read(&mut r).await?;
    let manifest: Vec<Value> =
        files.iter().map(|f| json!({"path": f.rel, "size": f.size, "sha256": f.sha256})).collect();
    w.write_all(format!("{}\n", json!({ "manifest": manifest })).as_bytes()).await?;
    let asked = read(&mut r).await?;
    let want: Vec<String> = serde_json::from_value(asked["want"].clone()).unwrap_or_default();
    for path in &want {
        let f = files
            .iter()
            .find(|f| &f.rel == path)
            .ok_or_else(|| anyhow::anyhow!("asked for {path}, not in the page"))?;
        w.write_all(format!("{}\n", json!({"file": f.rel, "size": f.size})).as_bytes()).await?;
        let mut src = tokio::fs::File::open(&f.full).await?;
        let sent = tokio::io::copy(&mut src, &mut w).await?;
        anyhow::ensure!(sent == f.size, "{} changed while it was being sent; push again", f.rel);
    }
    w.flush().await?;
    let done = read(&mut r).await?;
    let folder = done["folder"].as_str().unwrap_or_default();
    println!(
        "Pushed {}/{folder}: {} sent, {} unchanged, {} removed.",
        p.space, done["written"], done["kept"], done["removed"]
    );
    if let Some(url) = done["url"].as_str() {
        println!("{url}");
    }
    let problems = done["problems"].as_array().cloned().unwrap_or_default();
    if !problems.is_empty() {
        println!("Problems shown on the page:");
        for pr in problems {
            println!("  - {}", pr.as_str().unwrap_or_default());
        }
    }
    let _ = w.shutdown().await;
    Ok(())
}

/// One JSON line from the other side; `{"error"}` becomes the error.
async fn read<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> anyhow::Result<Value> {
    let mut line = String::new();
    anyhow::ensure!(r.read_line(&mut line).await? > 0, "the push was cut off before it finished");
    let v: Value = serde_json::from_str(&line).map_err(|e| anyhow::anyhow!("an unreadable answer ({e}): {line}"))?;
    if let Some(e) = v["error"].as_str() {
        anyhow::bail!("{e}");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_private_node_modules_and_links_stay_behind() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("2026-11-27-signing-qa");
        for dir in ["img", ".git", "_drafts", "node_modules/x"] {
            std::fs::create_dir_all(p.join(dir)).unwrap();
        }
        std::fs::write(p.join("index.md"), "# T\n\nS.\n").unwrap();
        std::fs::write(p.join("img/a.jpg"), "j").unwrap();
        std::fs::write(p.join(".git/config"), "x").unwrap();
        std::fs::write(p.join("_drafts/b.md"), "x").unwrap();
        std::fs::write(p.join("node_modules/x/i.js"), "x").unwrap();
        std::fs::write(p.join(".DS_Store"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", p.join("img/p")).unwrap();
        let (files, links) = collect(&p).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(names, ["img/a.jpg", "index.md"]);
        assert_eq!(links, ["img/p"]);
        assert_eq!(files[1].sha256.len(), 64);
        assert_eq!(default_name(&p).as_deref(), Some("signing-qa"));
    }

    #[test]
    fn a_folder_without_an_index_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.jpg"), "j").unwrap();
        assert!(collect(d.path()).unwrap_err().to_string().contains("index.md"));
    }
}
