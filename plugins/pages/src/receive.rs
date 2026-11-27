//! `fleetlayer-pages receive`: the writing end of `fleetlayer pages push`. The node that hosts a space runs it with its
//! stdin and stdout joined to the pushing machine's stream, after the control plane has checked `write` on
//! `page:<space>/<name>`. One push per run:
//!
//! 1. ← `{"manifest": [{"path", "size", "sha256"}]}`: every file of the page, relative to its folder.
//! 2. → `{"folder", "want": [paths]}`: the files that differ from what's already there.
//! 3. ← for each wanted file, a line `{"file", "size"}` and then exactly `size` bytes.
//! 4. → `{"done": true, "folder", "problems", "written", "kept", "removed"}`, or `{"error"}` at any point.
//!
//! New files are written under `.<folder>.incoming/` (never served) and checked against their sha256 before any is
//! moved in, so a broken push leaves the page as it was. Files the manifest doesn't list are removed from the folder.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::space::{self, SpaceConfig};

pub const MAX_FILES: usize = 200;
pub const MAX_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Deserialize)]
struct Manifest {
    manifest: Vec<Entry>,
}

#[derive(Deserialize)]
struct FileHeader {
    file: String,
    size: u64,
}

/// What a push asks for.
pub struct Request<'a> {
    pub space_name: &'a str,
    pub space: &'a SpaceConfig,
    /// The pages site's address (`base_url`), for the page's link in the answer.
    pub base_url: Option<&'a str>,
    /// The page's short name (`signing-flow-qa`): its folder is `<date>-<name>`.
    pub name: &'a str,
    /// Make a new dated folder even when one with this name exists.
    pub new: bool,
    /// `YYYY-MM-DD` for a new folder.
    pub today: &'a str,
}

/// A page name: lower-case letters, digits and hyphens, without a date in front.
pub fn valid_page_name(s: &str) -> bool {
    space::valid_name(s) && !s.ends_with('-') && space::folder_date(s).is_none()
}

/// A path in a manifest: relative, `/`-separated, served (no hidden or `_` parts), no `node_modules`.
fn valid_path(p: &str) -> bool {
    let path = Path::new(p);
    !p.is_empty()
        && p.len() <= 400
        && !p.contains('\\')
        && !p.contains('\0')
        && crate::render::servable(path)
        && path.components().all(|c| matches!(c, Component::Normal(s) if s != "node_modules"))
}

/// The folder this push writes: the newest existing `<date>-<name>` (or `<name>`), so its link stays the same, unless
/// `new`; else `<today>-<name>`.
pub fn folder_for(req: &Request) -> String {
    if !req.new
        && let Ok(rd) = std::fs::read_dir(&req.space.dir)
    {
        let mut found: Vec<String> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|f| f == req.name || (space::folder_date(f).is_some() && f.get(11..) == Some(req.name)))
            .collect();
        found.sort();
        if let Some(f) = found.pop() {
            return f;
        }
    }
    format!("{}-{}", req.today, req.name)
}

fn sha256_file(p: &Path) -> Option<String> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(hex::encode(h.finalize()))
}

/// Every served regular file under `dir`, relative to it: what a push could have written, and so may remove. Hidden and
/// `_` files are left alone.
fn files_under(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(dir.join(&rel)) else { continue };
        for e in rd.flatten() {
            let r = rel.join(e.file_name());
            if !crate::render::servable(&r) {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(r),
                Ok(t) if t.is_file() => {
                    if let Some(s) = r.to_str() {
                        out.insert(s.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn line<W: Write>(out: &mut W, v: serde_json::Value) -> std::io::Result<()> {
    writeln!(out, "{v}")?;
    out.flush()
}

/// Runs one push. Errors are sent as `{"error"}` before they're returned.
pub fn run<R: BufRead, W: Write>(req: &Request, input: &mut R, out: &mut W) -> anyhow::Result<serde_json::Value> {
    match push(req, input, out) {
        Ok(v) => Ok(v),
        Err(e) => {
            let _ = line(out, json!({"error": e.to_string()}));
            Err(e)
        }
    }
}

fn push<R: BufRead, W: Write>(req: &Request, input: &mut R, out: &mut W) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(valid_page_name(req.name), "page name {:?}: use lower-case letters, digits and hyphens", req.name);
    space::check("space", req.space).map_err(|e| anyhow::anyhow!(e))?;

    let mut first = String::new();
    input.read_line(&mut first)?;
    let m: Manifest = serde_json::from_str(&first).map_err(|e| anyhow::anyhow!("reading the manifest: {e}"))?;
    anyhow::ensure!(!m.manifest.is_empty(), "the page has no files");
    anyhow::ensure!(m.manifest.len() <= MAX_FILES, "the page has {} files; the limit is {MAX_FILES}", m.manifest.len());
    let total: u64 = m.manifest.iter().map(|e| e.size).sum();
    anyhow::ensure!(
        total <= MAX_BYTES,
        "the page is {} MB; the limit is {} MB",
        total / 1_000_000,
        MAX_BYTES / 1_000_000
    );
    let mut entries = BTreeMap::new();
    for e in m.manifest {
        anyhow::ensure!(
            valid_path(&e.path),
            "{:?} can't be part of a page (hidden, private or outside its folder)",
            e.path
        );
        anyhow::ensure!(
            e.sha256.len() == 64 && e.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "{}: the sha256 isn't 64 hex digits",
            e.path
        );
        let path = e.path.clone();
        anyhow::ensure!(entries.insert(path.clone(), e).is_none(), "{path} is listed twice");
    }
    anyhow::ensure!(
        entries.contains_key("index.md") || entries.contains_key("index.html"),
        "the page needs index.md (or index.html for a mock-up)"
    );

    let folder = folder_for(req);
    let root = req.space.dir.canonicalize()?;
    let dest = root.join(&folder);
    anyhow::ensure!(
        std::fs::symlink_metadata(&dest).map(|m| m.is_dir()).unwrap_or(true),
        "{folder} in the space is a file or a link, not a folder"
    );
    let existing = files_under(&dest);
    let mut want = vec![];
    let mut kept = 0;
    for (path, e) in &entries {
        let p = dest.join(path);
        // A symbolic link in the folder could point anywhere: never trust or follow one.
        // `existing` was listed without following links, so a file reached through a linked folder never counts.
        let same = existing.contains(path)
            && std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file() && m.len() == e.size)
            && sha256_file(&p).is_some_and(|h| h.eq_ignore_ascii_case(&e.sha256));
        if same {
            kept += 1;
        } else {
            want.push(path.clone());
        }
    }
    // Before asking for anything: a space the service can't write refuses here.
    let staging = root.join(format!(".{folder}.incoming"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(30) {
            anyhow::anyhow!(
                "this space doesn't accept pushes: the pages service can't write to its folder on its node (ask whoever \
                 runs it to make the space writable for pushes)"
            )
        } else {
            anyhow::anyhow!("making {}: {e}", staging.display())
        }
    })?;
    line(out, json!({"folder": folder, "want": want}))?;

    let staged = (|| -> anyhow::Result<()> {
        let mut left: BTreeSet<&String> = want.iter().collect();
        while !left.is_empty() {
            let mut h = String::new();
            anyhow::ensure!(input.read_line(&mut h)? > 0, "the push ended before every file arrived");
            let fh: FileHeader = serde_json::from_str(&h).map_err(|e| anyhow::anyhow!("reading a file header: {e}"))?;
            let e = entries.get(&fh.file).filter(|_| left.remove(&fh.file));
            let Some(e) = e else { anyhow::bail!("{:?} wasn't asked for", fh.file) };
            anyhow::ensure!(fh.size == e.size, "{}: {} bytes sent, the manifest says {}", fh.file, fh.size, e.size);
            let p = staging.join(&fh.file);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::File::create(&p)?;
            let mut hasher = Sha256::new();
            let mut limited = (&mut *input).take(fh.size);
            let mut buf = vec![0u8; 64 * 1024];
            let mut got = 0u64;
            loop {
                let n = limited.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                f.write_all(&buf[..n])?;
                got += n as u64;
            }
            anyhow::ensure!(got == fh.size, "{}: the push ended part-way through", fh.file);
            anyhow::ensure!(
                hex::encode(hasher.finalize()).eq_ignore_ascii_case(&e.sha256),
                "{}: the bytes don't match its sha256",
                fh.file
            );
        }
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // Everything arrived intact: move it in, then drop what the page no longer has.
    std::fs::create_dir_all(&dest)?;
    for path in &want {
        let to = dest.join(path);
        // Each folder on the way must be a real folder: a file or a symbolic link where one now goes is replaced, so
        // nothing is written through a link to somewhere else.
        let mut at = dest.clone();
        for part in Path::new(path).parent().into_iter().flat_map(Path::components) {
            at.push(part);
            if std::fs::symlink_metadata(&at).is_ok_and(|m| !m.is_dir()) {
                std::fs::remove_file(&at)?;
            }
            if !at.exists() {
                std::fs::create_dir(&at)?;
            }
        }
        if std::fs::symlink_metadata(&to).is_ok_and(|m| m.is_dir()) {
            std::fs::remove_dir_all(&to)?;
        }
        std::fs::rename(staging.join(path), &to)?;
    }
    let mut removed = 0;
    for path in existing.iter().filter(|p| !entries.contains_key(*p)) {
        if std::fs::remove_file(dest.join(path)).is_ok() {
            removed += 1;
        }
    }
    remove_empty_dirs(&dest);
    let _ = std::fs::remove_dir_all(&staging);

    let rendered = crate::render::page(&root, Path::new(&folder));
    let problems: Vec<String> = rendered.problems.iter().map(|p| p.message.clone()).collect();
    let url = req.base_url.map(|b| format!("{}/{}/{folder}/", b.trim_end_matches('/'), req.space_name));
    let done = json!({
        "done": true,
        "folder": folder,
        "url": url,
        "problems": problems,
        "written": want.len(),
        "kept": kept,
        "removed": removed,
    });
    line(out, done.clone())?;
    Ok(done)
}

/// Removes folders left empty under `dir` (not `dir` itself), skipping hidden ones.
fn remove_empty_dirs(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    let mut empty = true;
    for e in rd.flatten() {
        let p = e.path();
        let hidden = !crate::render::servable(Path::new(&e.file_name()));
        if !hidden && e.file_type().is_ok_and(|t| t.is_dir()) && remove_empty_dirs(&p) {
            let _ = std::fs::remove_dir(&p);
        } else {
            empty = false;
        }
    }
    empty
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> (tempfile::TempDir, SpaceConfig) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(space::MARKER), b"").unwrap();
        let cfg = SpaceConfig {
            dir: d.path().to_path_buf(),
            title: None,
            owner: None,
            tenant: None,
            apps: None,
            untrusted: false,
        };
        (d, cfg)
    }

    fn entry(path: &str, data: &[u8]) -> Entry {
        Entry { path: path.into(), size: data.len() as u64, sha256: hex::encode(Sha256::digest(data)) }
    }

    /// A full push as the client sends it: the manifest, then the files the receiver asks for.
    fn push_files(
        cfg: &SpaceConfig,
        files: &[(&str, &[u8])],
        new: bool,
    ) -> (anyhow::Result<serde_json::Value>, Vec<String>) {
        let manifest: Vec<Entry> = files.iter().map(|(p, d)| entry(p, d)).collect();
        // The input is prepared up front, so send the files whose content differs from what's there: what it will ask for.
        let req = Request {
            space_name: "s",
            space: cfg,
            base_url: Some("https://p.example"),
            name: "qa",
            new,
            today: "2026-11-27",
        };
        let folder = folder_for(&req);
        let dest = cfg.dir.join(&folder);
        let mut input = format!("{}\n", json!({"manifest": manifest})).into_bytes();
        for (p, d) in files {
            let same = sha256_file(&dest.join(p)).is_some_and(|h| h == hex::encode(Sha256::digest(d)));
            if !same {
                input.extend(format!("{}\n", json!({"file": p, "size": d.len()})).bytes());
                input.extend_from_slice(d);
            }
        }
        let mut out = vec![];
        let r = run(&req, &mut std::io::Cursor::new(input), &mut out);
        (r, String::from_utf8(out).unwrap().lines().map(str::to_string).collect())
    }

    const MD: &[u8] = b"# QA\n\nWhat it shows.\n\n![Light](img/a.jpg)\n";

    #[test]
    fn a_first_push_writes_a_dated_folder_and_reports_its_problems() {
        let (_d, cfg) = space();
        let (r, lines) = push_files(&cfg, &[("index.md", MD), ("img/a.jpg", b"jpeg")], false);
        let v = r.unwrap();
        assert_eq!(v["folder"], "2026-11-27-qa");
        assert_eq!(v["url"], "https://p.example/s/2026-11-27-qa/");
        assert_eq!(v["written"], 2);
        assert_eq!(v["problems"].as_array().unwrap().len(), 0, "{v}");
        assert!(lines[0].contains("\"want\""));
        assert_eq!(std::fs::read(cfg.dir.join("2026-11-27-qa/img/a.jpg")).unwrap(), b"jpeg");
        assert!(!cfg.dir.join(".2026-11-27-qa.incoming").exists());
    }

    #[test]
    fn a_second_push_sends_only_changes_keeps_the_folder_and_drops_removed_files() {
        let (_d, cfg) = space();
        std::fs::create_dir_all(cfg.dir.join("2026-11-20-qa/img")).unwrap();
        std::fs::write(cfg.dir.join("2026-11-20-qa/index.md"), MD).unwrap();
        std::fs::write(cfg.dir.join("2026-11-20-qa/img/a.jpg"), b"jpeg").unwrap();
        std::fs::write(cfg.dir.join("2026-11-20-qa/img/old.jpg"), b"old").unwrap();
        let (r, lines) = push_files(&cfg, &[("index.md", MD), ("img/a.jpg", b"jpeg2")], false);
        let v = r.unwrap();
        assert_eq!(v["folder"], "2026-11-20-qa");
        assert_eq!((v["written"].as_u64(), v["kept"].as_u64(), v["removed"].as_u64()), (Some(1), Some(1), Some(1)));
        assert!(lines[0].contains("[\"img/a.jpg\"]"), "{}", lines[0]);
        assert!(!cfg.dir.join("2026-11-20-qa/img/old.jpg").exists());
        assert!(!cfg.dir.join("2026-11-27-qa").exists());
    }

    #[test]
    fn new_makes_a_fresh_folder() {
        let (_d, cfg) = space();
        std::fs::create_dir_all(cfg.dir.join("2026-11-20-qa")).unwrap();
        let (r, _) = push_files(&cfg, &[("index.md", MD), ("img/a.jpg", b"j")], true);
        assert_eq!(r.unwrap()["folder"], "2026-11-27-qa");
    }

    #[test]
    fn bytes_that_dont_match_their_hash_change_nothing() {
        let (_d, cfg) = space();
        std::fs::create_dir_all(cfg.dir.join("2026-11-27-qa")).unwrap();
        std::fs::write(cfg.dir.join("2026-11-27-qa/index.md"), b"# Old\n\nold.\n").unwrap();
        let req = Request { space_name: "s", space: &cfg, base_url: None, name: "qa", new: false, today: "2026-11-27" };
        let m = vec![entry("index.md", MD)];
        let mut input = format!("{}\n", json!({"manifest": m})).into_bytes();
        input.extend(format!("{}\n", json!({"file": "index.md", "size": MD.len()})).bytes());
        let mut bad = MD.to_vec();
        bad[0] = b'X';
        input.extend_from_slice(&bad);
        let mut out = vec![];
        let err = run(&req, &mut std::io::Cursor::new(input), &mut out).unwrap_err();
        assert!(err.to_string().contains("sha256"), "{err}");
        assert!(String::from_utf8(out).unwrap().contains("\"error\""));
        assert_eq!(std::fs::read(cfg.dir.join("2026-11-27-qa/index.md")).unwrap(), b"# Old\n\nold.\n");
        assert!(!cfg.dir.join(".2026-11-27-qa.incoming").exists());
    }

    #[test]
    fn a_push_that_stops_early_changes_nothing() {
        let (_d, cfg) = space();
        let req = Request { space_name: "s", space: &cfg, base_url: None, name: "qa", new: false, today: "2026-11-27" };
        let m = vec![entry("index.md", MD)];
        let mut input = format!("{}\n", json!({"manifest": m})).into_bytes();
        input.extend(format!("{}\n", json!({"file": "index.md", "size": MD.len()})).bytes());
        input.extend_from_slice(&MD[..5]);
        let err = run(&req, &mut std::io::Cursor::new(input), &mut vec![]).unwrap_err();
        assert!(err.to_string().contains("part-way"), "{err}");
        assert!(!cfg.dir.join("2026-11-27-qa/index.md").exists());
    }

    #[test]
    fn paths_outside_the_folder_hidden_files_and_node_modules_are_refused() {
        let (_d, cfg) = space();
        for bad in ["../x.md", "/etc/passwd", ".git/config", "_drafts/a.md", "node_modules/a.js", "a/../../b", "a\\b"] {
            let (r, _) = push_files(&cfg, &[("index.md", MD), (bad, b"x")], false);
            assert!(r.is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn limits_and_names_are_checked() {
        let (_d, cfg) = space();
        let many: Vec<(String, Vec<u8>)> = (0..=MAX_FILES).map(|i| (format!("img/{i}.jpg"), vec![b'x'])).collect();
        let mut files: Vec<(&str, &[u8])> = many.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
        files.push(("index.md", MD));
        let (r, _) = push_files(&cfg, &files, false);
        assert!(r.unwrap_err().to_string().contains("limit"));
        let (r, _) = push_files(&cfg, &[("img/a.jpg", b"x")], false);
        assert!(r.unwrap_err().to_string().contains("index.md"));
        assert!(!valid_page_name("2026-11-27-qa"));
        assert!(!valid_page_name("Q A"));
        assert!(valid_page_name("signing-flow-qa"));
    }

    #[test]
    fn a_linked_subfolder_is_replaced_not_written_through() {
        let (_d, cfg) = space();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cfg.dir.join("2026-11-27-qa")).unwrap();
        std::os::unix::fs::symlink(outside.path(), cfg.dir.join("2026-11-27-qa/img")).unwrap();
        let (r, _) = push_files(&cfg, &[("index.md", MD), ("img/a.jpg", b"j")], false);
        r.unwrap();
        assert!(!outside.path().join("a.jpg").exists());
        assert!(cfg.dir.join("2026-11-27-qa/img").symlink_metadata().unwrap().is_dir());
    }

    #[test]
    fn a_symbolic_link_in_the_folder_is_replaced_not_followed() {
        let (d, cfg) = space();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"keep").unwrap();
        std::fs::create_dir_all(cfg.dir.join("2026-11-27-qa")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), cfg.dir.join("2026-11-27-qa/index.md")).unwrap();
        let (r, _) = push_files(&cfg, &[("index.md", MD), ("img/a.jpg", b"j")], false);
        r.unwrap();
        assert_eq!(std::fs::read(outside.path().join("secret")).unwrap(), b"keep");
        assert!(!std::fs::symlink_metadata(d.path().join("2026-11-27-qa/index.md")).unwrap().file_type().is_symlink());
    }
}
