use super::*;
use std::os::unix::fs::symlink;
use std::time::Duration;

fn set_mtime(path: &std::path::Path, secs_ago: u64) {
    let f = std::fs::File::options().append(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago)).unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    server: Server,
}

/// A screenshots folder (marked trusted, so replies carry no outside-data line) with three files of different ages, a hidden file, a subfolder and two symbolic links (one
/// pointing out of the folder), next to a secret file outside it.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let shots = root.join("shots");
    std::fs::create_dir(&shots).unwrap();
    std::fs::write(root.join("secret.txt"), "outside").unwrap();
    std::fs::write(shots.join("old.txt"), "old notes").unwrap();
    set_mtime(&shots.join("old.txt"), 3 * 86_400);
    std::fs::write(shots.join("middle.bin"), [0u8, 1, 2, 3, 255]).unwrap();
    set_mtime(&shots.join("middle.bin"), 7200);
    let img = image::RgbImage::from_fn(3000, 1500, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
    img.save(shots.join("Screenshot 2026-11-27 at 09.00.00.png")).unwrap();
    set_mtime(&shots.join("Screenshot 2026-11-27 at 09.00.00.png"), 30);
    std::fs::write(shots.join(".hidden"), "x").unwrap();
    std::fs::create_dir(shots.join("sub")).unwrap();
    std::fs::write(shots.join("sub/inner.txt"), "inner").unwrap();
    symlink(root.join("secret.txt"), shots.join("escape.txt")).unwrap();
    symlink(shots.join("old.txt"), shots.join("alias.txt")).unwrap();
    let folders = vec![Folder { name: "screenshots".into(), path: shots, trusted: true, upload: false, delete: false }];
    Fixture { _dir: dir, root, server: Server { folders, max_bytes: 8 * 1024 * 1024, min_free: 0 } }
}

fn call(s: &Server, tool: &str, args: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}});
    s.handle(&line.to_string()).unwrap()["result"].clone()
}

fn text(result: &Value, i: usize) -> String {
    result["content"][i]["text"].as_str().unwrap_or_default().to_string()
}

fn refused(result: &Value) -> String {
    assert_eq!(result["isError"], true, "expected a refusal, got {result}");
    text(result, 0)
}

#[test]
fn folders_are_name_equals_path_with_the_home_expanded() {
    let home = std::path::Path::new("/Users/me");
    let f = parse_folders(&["screenshots=~/Screenshots".into(), "dl=/tmp/dl".into()], Some(home)).unwrap();
    assert_eq!(f[0].path, PathBuf::from("/Users/me/Screenshots"));
    assert_eq!(f[1].name, "dl");
    assert!(parse_folders(&["nope".into()], Some(home)).is_err());
    assert!(parse_folders(&["a=relative/path".into()], Some(home)).is_err());
    assert!(parse_folders(&["a b=/x".into()], Some(home)).is_err());
    assert!(parse_folders(&["a=/x".into(), "a=/y".into()], Some(home)).is_err());
    assert!(parse_folders(&["a=~other/x".into()], Some(home)).is_err());
}

#[test]
fn speaks_mcp_and_stays_quiet_on_notifications() {
    let f = fixture();
    let init = f
        .server
        .handle(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}"#)
        .unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    let unknown = f
        .server
        .handle(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#)
        .unwrap();
    assert_eq!(unknown["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    assert!(f.server.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    let list = f.server.handle(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#).unwrap();
    let names: Vec<&str> =
        list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["folders", "list", "latest", "find", "read"]);
    assert_eq!(
        f.server.handle(r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#).unwrap()["error"]["code"],
        -32601
    );
    assert_eq!(f.server.handle("not json").unwrap()["error"]["code"], -32700);
    assert_eq!(f.server.handle(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#).unwrap()["result"], json!({}));
}

#[test]
fn long_request_lines_are_dropped_not_buffered() {
    let mut input = vec![b'x'; MAX_REQUEST + 10];
    input.extend_from_slice(b"\n{\"ok\":1}\n");
    let mut r = std::io::BufReader::with_capacity(1024, &input[..]);
    assert!(matches!(read_line(&mut r).unwrap(), Line::TooLong));
    assert!(matches!(read_line(&mut r).unwrap(), Line::Text(t) if t.trim() == "{\"ok\":1}"));
    assert!(matches!(read_line(&mut r).unwrap(), Line::Eof));
}

#[test]
fn lists_visible_files_newest_first_and_skips_links_folders_and_hidden_files() {
    let f = fixture();
    let r = call(&f.server, "list", json!({"folder": "screenshots"}));
    let v: Value = serde_json::from_str(&text(&r, 0)).unwrap();
    let names: Vec<&str> =
        v["listing"]["files"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Screenshot 2026-11-27 at 09.00.00.png", "middle.bin", "old.txt"]);
    assert_eq!(v["listing"]["skipped_links"], 2);
    assert_eq!(v["listing"]["files"][0]["kind"], "image");
    assert_eq!(v["listing"]["files"][0]["ago"], "just now");
    assert_eq!(v["listing"]["files"][2]["ago"], "3 days ago");
    // The only folder may be left out.
    assert_eq!(call(&f.server, "list", json!({}))["isError"], Value::Null);
    assert!(refused(&call(&f.server, "list", json!({"folder": "desktop"}))).contains("screenshots"));
}

#[test]
fn latest_finds_the_screenshot_just_taken_and_can_show_it() {
    let f = fixture();
    let r = call(&f.server, "latest", json!({"folder": "screenshots", "kind": "image", "read": true}));
    let v: Value = serde_json::from_str(&text(&r, 0)).unwrap();
    assert_eq!(v["listing"]["files"][0]["name"], "Screenshot 2026-11-27 at 09.00.00.png");
    assert!(text(&r, 1).contains("3000x1500 sent as 1568x784"), "{}", text(&r, 1));
    assert_eq!(r["content"][2]["type"], "image");
    assert_eq!(r["content"][2]["mimeType"], "image/png");
    let png = base64::engine::general_purpose::STANDARD.decode(r["content"][2]["data"].as_str().unwrap()).unwrap();
    let img = image::load_from_memory(&png).unwrap();
    assert_eq!((img.width(), img.height()), (1568, 784));
    let none = call(&f.server, "latest", json!({"kind": "pdf"}));
    assert!(refused(&none).contains("no pdf files"));
}

use base64::Engine;

#[test]
fn reads_images_at_their_own_size_when_asked() {
    let f = fixture();
    let r = call(&f.server, "read", json!({"name": "Screenshot 2026-11-27 at 09.00.00.png", "max_px": 0}));
    let png = base64::engine::general_purpose::STANDARD.decode(r["content"][1]["data"].as_str().unwrap()).unwrap();
    assert_eq!(png, std::fs::read(f.server.folders[0].path.join("Screenshot 2026-11-27 at 09.00.00.png")).unwrap());
}

#[test]
fn reads_text_as_text_and_other_files_as_base64() {
    let f = fixture();
    let r = call(&f.server, "read", json!({"name": "old.txt"}));
    assert_eq!(text(&r, 1), "old notes");
    let r = call(&f.server, "read", json!({"name": "middle.bin"}));
    assert_eq!(r["content"][1]["type"], "resource");
    assert_eq!(r["content"][1]["resource"]["blob"], "AAECA/8=");
    assert_eq!(r["content"][1]["resource"]["uri"], "folder://screenshots/middle.bin");
}

#[test]
fn never_reads_outside_the_folder() {
    let f = fixture();
    for name in ["../secret.txt", "sub/inner.txt", "/etc/passwd", "..", ".", ".hidden", ""] {
        let r = call(&f.server, "read", json!({"name": name}));
        let why = refused(&r);
        assert!(!why.contains("outside"), "{name}: {why}");
    }
    assert!(refused(&call(&f.server, "read", json!({"name": "escape.txt"}))).contains("symbolic link"));
    assert!(refused(&call(&f.server, "read", json!({"name": "alias.txt"}))).contains("symbolic link"));
    assert!(refused(&call(&f.server, "read", json!({"name": "sub"}))).contains("isn't a file"));
    assert!(refused(&call(&f.server, "read", json!({"name": "missing.png"}))).contains("no file called"));
    assert!(f.root.join("secret.txt").exists());
}

#[test]
fn a_folder_that_is_itself_a_link_is_followed_once_at_its_root() {
    // ~/Screenshots may be a link to somewhere else; the files inside it are still confined.
    let f = fixture();
    let link = f.root.join("shots-link");
    symlink(&f.server.folders[0].path, &link).unwrap();
    let s = Server {
        folders: vec![Folder { name: "s".into(), path: link, trusted: true, upload: false, delete: false }],
        max_bytes: 1024 * 1024,
        min_free: 0,
    };
    assert_eq!(text(&call(&s, "read", json!({"name": "old.txt"})), 1), "old notes");
    assert!(refused(&call(&s, "read", json!({"name": "escape.txt"}))).contains("symbolic link"));
}

#[test]
fn big_files_are_refused_with_their_size() {
    let f = fixture();
    let shots = &f.server.folders[0].path;
    std::fs::write(shots.join("big.zip"), vec![7u8; 2 * 1024 * 1024]).unwrap();
    let small = Server { folders: f.server.folders.clone(), max_bytes: 1024 * 1024, min_free: 0 };
    assert!(refused(&call(&small, "read", json!({"name": "big.zip"}))).contains("2.0 MB"));
    // Long text is cut, with a note.
    std::fs::write(shots.join("long.log"), "é".repeat(folders::MAX_TEXT)).unwrap();
    let r = call(&f.server, "read", json!({"name": "long.log"}));
    assert!(text(&r, 0).contains("the first"));
    assert!(text(&r, 1).len() <= folders::MAX_TEXT);
}

#[test]
fn a_broken_or_hostile_image_is_sent_as_a_file_not_a_crash() {
    let f = fixture();
    let shots = &f.server.folders[0].path;
    std::fs::write(shots.join("broken.png"), b"\x89PNG\r\n\x1a\nnot really").unwrap();
    let r = call(&f.server, "read", json!({"name": "broken.png"}));
    assert!(text(&r, 0).contains("can't preview"), "{r}");
    assert_eq!(r["content"][1]["type"], "resource");
    // A PNG header claiming 100000 x 100000 pixels is refused by the decoder's limits, not allocated.
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend_from_slice(&100_000u32.to_be_bytes());
    ihdr.extend_from_slice(&100_000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut header = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0d".to_vec();
    header.extend_from_slice(&ihdr);
    header.extend_from_slice(&crc32(&ihdr).to_be_bytes());
    std::fs::write(shots.join("huge.png"), header).unwrap();
    let r = call(&f.server, "read", json!({"name": "huge.png"}));
    assert!(text(&r, 0).to_lowercase().contains("limit"), "{r}");
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[test]
fn finds_by_pattern_and_time() {
    let f = fixture();
    let names = |r: &Value| -> Vec<String> {
        let v: Value = serde_json::from_str(&text(r, 0)).unwrap();
        v["listing"]["files"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(
        names(&call(&f.server, "find", json!({"pattern": "screenshot*.PNG"}))),
        ["Screenshot 2026-11-27 at 09.00.00.png"]
    );
    assert_eq!(names(&call(&f.server, "find", json!({"pattern": "old"}))), ["old.txt"]);
    assert_eq!(
        names(&call(&f.server, "find", json!({"since": "3h"}))),
        ["Screenshot 2026-11-27 at 09.00.00.png", "middle.bin"]
    );
    assert_eq!(names(&call(&f.server, "find", json!({"pattern": "*.txt", "since": "1h"}))), Vec::<String>::new());
    assert!(refused(&call(&f.server, "find", json!({}))).contains("pattern"));
    assert!(refused(&call(&f.server, "find", json!({"since": "yesterday"}))).contains("isn't a time"));
}

#[test]
fn globs_and_times() {
    use folders::name_matches as m;
    assert!(m("*.png", "A.PNG"));
    assert!(m("Screen?hot*", "screenshot 1.png"));
    assert!(!m("*.png", "a.png.zip"));
    assert!(m("*a*b*", "xxaYYbzz"));
    assert!(m("invoice", "My Invoice 2026.pdf"));
    let now = SystemTime::now();
    assert_eq!(folders::parse_since("2h", now).unwrap(), now - Duration::from_secs(7200));
    assert!(folders::parse_since("2026-11-27T09:00:00Z", now).is_ok());
    assert!(folders::parse_since("2026-11-27", now).is_ok());
    assert!(folders::parse_since("2x", now).is_err());
}

#[test]
fn a_missing_folder_is_an_error_that_says_so() {
    let s = Server {
        folders: vec![Folder {
            name: "gone".into(),
            path: "/nonexistent/fleetlayer".into(),
            trusted: false,
            upload: false,
            delete: false,
        }],
        max_bytes: 1024,
        min_free: 0,
    };
    assert!(refused(&call(&s, "list", json!({}))).contains("doesn't exist"));
    let r = call(&s, "folders", json!({}));
    assert!(text(&r, 0).contains("doesn't exist"));
}

#[test]
fn what_is_read_from_an_untrusted_folder_is_marked_as_outside_data_once() {
    let mut f = fixture();
    f.server.folders[0].trusted = false;
    let r = call(&f.server, "read", json!({"name": "old.txt"}));
    assert_eq!(text(&r, 0), OUTSIDE_DATA);
    assert_eq!(text(&r, 2), "old notes");
    let r = call(&f.server, "latest", json!({"n": 3, "read": true}));
    let items = r["content"].as_array().unwrap();
    assert_eq!(items.iter().filter(|i| i["text"] == OUTSIDE_DATA).count(), 1);
    assert_eq!(items[1]["text"], OUTSIDE_DATA, "after the listing, ahead of every file");
    // Listings carry no file contents, so no line.
    assert!(!call(&f.server, "list", json!({}))["content"].to_string().contains("outside data"));
    // A refusal isn't content either.
    assert!(!refused(&call(&f.server, "read", json!({"name": "missing"}))).contains("outside data"));

    let mut folders = f.server.folders.clone();
    grant(&mut folders, "--trusted", &["screenshots".into()], |f| f.trusted = true).unwrap();
    assert!(folders[0].trusted);
    assert!(grant(&mut folders, "--upload", &["downloads".into()], |f| f.upload = true).is_err());
}

/// The fixture's folder with uploads and/or deleting allowed.
fn writable(f: &Fixture, upload: bool, delete: bool) -> Server {
    let mut folder = f.server.folders[0].clone();
    folder.upload = upload;
    folder.delete = delete;
    Server { folders: vec![folder], max_bytes: 1024 * 1024, min_free: 0 }
}

fn tool_names(s: &Server) -> Vec<String> {
    let list = s.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
    list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

#[test]
fn upload_and_delete_exist_only_where_a_folder_allows_them() {
    let f = fixture();
    assert!(!tool_names(&f.server).contains(&"upload".to_string()));
    assert!(refused(&call(&f.server, "upload", json!({"name": "x.txt", "text": "x"}))).contains("no tool called"));
    assert_eq!(tool_names(&writable(&f, true, false)).last().unwrap(), "upload");
    assert_eq!(tool_names(&writable(&f, false, true)).last().unwrap(), "delete");
    // A second folder without the right refuses, naming the flag.
    let mut s = writable(&f, true, true);
    s.folders.push(Folder { name: "ro".into(), path: f.root.clone(), trusted: false, upload: false, delete: false });
    assert!(
        refused(&call(&s, "upload", json!({"folder": "ro", "name": "a.txt", "text": "a"}))).contains("--upload ro")
    );
    assert!(refused(&call(&s, "delete", json!({"folder": "ro", "name": "secret.txt"}))).contains("--delete ro"));
    assert!(f.root.join("secret.txt").exists());
}

#[test]
fn uploads_land_whole_and_never_replace_a_file_unasked() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    let s = writable(&f, true, false);
    let dir = &s.folders[0].path;
    let r = call(&s, "upload", json!({"name": "report.md", "text": "# Hi"}));
    assert!(text(&r, 0).contains("wrote report.md"), "{r}");
    assert_eq!(std::fs::read_to_string(dir.join("report.md")).unwrap(), "# Hi");
    assert_eq!(std::fs::metadata(dir.join("report.md")).unwrap().permissions().mode() & 0o777, 0o644);
    call(&s, "upload", json!({"name": "blob.bin", "base64": "AAECA/8="}));
    assert_eq!(std::fs::read(dir.join("blob.bin")).unwrap(), [0, 1, 2, 3, 255]);
    // No temporary files left behind.
    assert!(
        !std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(".fleetlayer-upload"))
    );

    assert!(refused(&call(&s, "upload", json!({"name": "old.txt", "text": "new"}))).contains("already exists"));
    assert!(
        refused(&call(&s, "upload", json!({"name": "old.txt", "text": "new", "overwrite": true}))).contains("--delete")
    );
    assert_eq!(std::fs::read_to_string(dir.join("old.txt")).unwrap(), "old notes");
    for name in ["../escape.txt", ".bashrc", "sub/x", ""] {
        refused(&call(&s, "upload", json!({"name": name, "text": "x"})));
    }
    assert!(!f.root.join("escape.txt").exists());
    assert!(
        refused(&call(&s, "upload", json!({"name": "escape.txt", "text": "x", "overwrite": true})))
            .contains("--delete")
    );
    assert!(refused(&call(&s, "upload", json!({"name": "sub", "text": "x"}))).contains("isn't a regular file"));
    assert!(refused(&call(&s, "upload", json!({"name": "a", "base64": "!!"}))).contains("base64"));
    assert!(refused(&call(&s, "upload", json!({"name": "a"}))).contains("text or as base64"));
    let big = "x".repeat(1024 * 1024 + 1);
    assert!(refused(&call(&s, "upload", json!({"name": "big.txt", "text": big}))).contains("more than"));
}

#[test]
fn overwriting_keeps_the_old_file_in_the_trash() {
    let f = fixture();
    let s = writable(&f, true, true);
    let dir = &s.folders[0].path;
    let r = call(&s, "upload", json!({"name": "old.txt", "text": "new notes", "overwrite": true}));
    assert!(text(&r, 0).contains(".fleetlayer-trash/"), "{r}");
    assert_eq!(std::fs::read_to_string(dir.join("old.txt")).unwrap(), "new notes");
    let trashed: Vec<_> = std::fs::read_dir(dir.join(folders::TRASH)).unwrap().flatten().collect();
    assert_eq!(trashed.len(), 1);
    assert_eq!(std::fs::read_to_string(trashed[0].path()).unwrap(), "old notes");
    // Symbolic links are never overwritten, even with overwrite: true.
    assert!(
        refused(&call(&s, "upload", json!({"name": "escape.txt", "text": "x", "overwrite": true})))
            .contains("isn't a regular file")
    );
    assert_eq!(std::fs::read_to_string(f.root.join("secret.txt")).unwrap(), "outside");
}

#[test]
fn an_upload_that_would_nearly_fill_the_disk_is_refused() {
    let f = fixture();
    let mut s = writable(&f, true, false);
    s.min_free = u64::MAX / 2;
    let why = refused(&call(&s, "upload", json!({"name": "a.txt", "text": "a"})));
    assert!(why.contains("free") && why.contains("keeps at least"), "{why}");
    assert!(!s.folders[0].path.join("a.txt").exists());
    assert!(folders::free_bytes(&s.folders[0].path).unwrap() > 0);
}

#[test]
fn deleting_moves_to_the_trash_and_says_where() {
    let f = fixture();
    let s = writable(&f, false, true);
    let dir = &s.folders[0].path;
    let r = call(&s, "delete", json!({"name": "old.txt"}));
    assert!(text(&r, 0).contains("kept 30 days"), "{r}");
    assert!(!dir.join("old.txt").exists());
    let trashed: Vec<String> = std::fs::read_dir(dir.join(folders::TRASH))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(trashed[0].ends_with("-old.txt"));
    assert!(folders::trashed_at(&trashed[0]).is_some(), "{}", trashed[0]);
    // The trash isn't listed.
    assert!(!call(&s, "list", json!({}))["content"].to_string().contains("fleetlayer-trash"));
    assert!(refused(&call(&s, "delete", json!({"name": "escape.txt"}))).contains("symbolic link"));
    assert!(refused(&call(&s, "delete", json!({"name": "sub"}))).contains("isn't a file"));
    assert!(refused(&call(&s, "delete", json!({"name": "../secret.txt"}))).contains("isn't a file name"));
    assert!(refused(&call(&s, "delete", json!({"name": ".fleetlayer-trash"}))).contains("hidden"));
    assert!(
        f.root.join("secret.txt").exists() && dir.join("escape.txt").exists() && dir.join("sub/inner.txt").exists()
    );
}

#[test]
fn a_file_that_disappears_between_list_and_delete_is_a_clear_refusal() {
    let f = fixture();
    let s = writable(&f, false, true);
    assert!(call(&s, "list", json!({}))["content"].to_string().contains("middle.bin"));
    std::fs::remove_file(s.folders[0].path.join("middle.bin")).unwrap();
    assert!(refused(&call(&s, "delete", json!({"name": "middle.bin"}))).contains("no file called"));
}

#[test]
fn the_trash_is_purged_after_30_days_by_the_time_in_each_name() {
    let f = fixture();
    let s = writable(&f, false, true);
    let folder = &s.folders[0];
    call(&s, "delete", json!({"name": "old.txt"}));
    let trash = folder.path.join(folders::TRASH);
    std::fs::write(trash.join("2026-01-01T00-00-00.000Z-ancient.txt"), "x").unwrap();
    std::fs::write(trash.join("not-a-trashed-name.txt"), "x").unwrap();
    std::fs::write(folder.path.join(".fleetlayer-upload-1-2"), "half").unwrap();
    set_mtime(&folder.path.join(".fleetlayer-upload-1-2"), 2 * 86_400);
    // old.txt was modified 3 days ago, but it was trashed just now: it stays.
    assert_eq!(folders::purge_trash(folder, SystemTime::now()), 1);
    let left: Vec<String> =
        std::fs::read_dir(&trash).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(left.iter().any(|n| n.ends_with("-old.txt")));
    assert!(!folder.path.join(".fleetlayer-upload-1-2").exists());
    // 31 days on, the rest of what it trashed goes too; names it didn't write stay.
    assert_eq!(folders::purge_trash(folder, SystemTime::now() + Duration::from_secs(31 * 86_400)), 1);
    assert!(trash.join("not-a-trashed-name.txt").exists());
}
