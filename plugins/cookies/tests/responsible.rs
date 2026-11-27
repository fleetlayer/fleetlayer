//! The built program relaunches itself as its own responsible process on macOS, so privacy grants (Full Disk Access)
//! are asked of it and not of the node that starts it.

#[cfg(target_os = "macos")]
#[test]
fn it_is_its_own_responsible_process_on_macos() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fleetlayer-cookies")).arg("responsible").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text} {}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("is its own responsible process"), "{text} {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn the_plugin_still_speaks_on_stdio_after_relaunching() {
    use std::io::{BufRead, BufReader, Write};
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_fleetlayer-cookies"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"_fleetlayer.dev/ping","params":{{}}}}"#).unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    assert!(line.contains(r#""id":1"#) && line.contains("result"), "{line}");
    // Closing stdin stops it, relaunched copy and all.
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
