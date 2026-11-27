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

/// A screenshots folder with three files of different ages, a hidden file, a subfolder and two symbolic links (one
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
    let folders = vec![Folder { name: "screenshots".into(), path: shots }];
    Fixture { _dir: dir, root, server: Server { folders, max_bytes: 8 * 1024 * 1024 } }
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
    let s = Server { folders: vec![Folder { name: "s".into(), path: link }], max_bytes: 1024 * 1024 };
    assert_eq!(text(&call(&s, "read", json!({"name": "old.txt"})), 1), "old notes");
    assert!(refused(&call(&s, "read", json!({"name": "escape.txt"}))).contains("symbolic link"));
}

#[test]
fn big_files_are_refused_with_their_size() {
    let f = fixture();
    let shots = &f.server.folders[0].path;
    std::fs::write(shots.join("big.zip"), vec![7u8; 2 * 1024 * 1024]).unwrap();
    let small = Server { folders: f.server.folders.clone(), max_bytes: 1024 * 1024 };
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
        folders: vec![Folder { name: "gone".into(), path: "/nonexistent/fleetlayer".into() }],
        max_bytes: 1024,
    };
    assert!(refused(&call(&s, "list", json!({}))).contains("doesn't exist"));
    let r = call(&s, "folders", json!({}));
    assert!(text(&r, 0).contains("doesn't exist"));
}
