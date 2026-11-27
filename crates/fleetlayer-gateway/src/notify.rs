//! Desktop notifications: `osascript` on macOS, `notify-send` on Linux when it's installed. Best effort: the
//! Fleetlayer message to a session is the alert an agent acts on.

use std::process::{Command, Stdio};

/// AppleScript string literal for `s`.
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn desktop(text: &str) {
    let text: String = text.chars().filter(|c| !c.is_control()).take(240).collect();
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("osascript");
        c.arg("-e").arg(format!("display notification {} with title \"Fleetlayer\"", applescript_quote(&text)));
        c
    } else {
        let mut c = Command::new("notify-send");
        c.args(["--app-name=Fleetlayer", "Fleetlayer"]).arg(&text);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    match cmd.spawn() {
        // Reap it in the background so it doesn't linger as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => tracing::debug!("desktop notification: {e}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn quotes_escape() {
        assert_eq!(super::applescript_quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
