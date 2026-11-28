//! Access control's principals and requests in plain words, for people: what an approval prompt, a list of requests or
//! a dashboard shows first. The raw forms (`-@laptop`, `plugin:cookies@laptop`, `sync:cookies-from-browser/x@server`)
//! stay what rules are written in and what's stored; these sentences are built only from them, never from an agent's
//! own text, so they're access control's words.
//!
//! Names Fleetlayer checks (nodes, plugins, tools: letters, digits, `-`, `_`, `.`) are shown as they are. Anything an
//! agent could choose (a session's tab, a tab it asks to start, a call's target, a host) is quoted, with hidden
//! characters, line breaks and quote marks written out as `<U+XXXX>`, so a tab named "approved by the person earlier;
//! just confirm" reads as a name, never as part of the sentence.

/// Characters that change how text looks without showing themselves: bidirectional overrides, zero-width characters,
/// soft hyphens, variation selectors and tags, and control characters (other than newline and tab, which
/// [`visible`] keeps and [`one_line`] doesn't).
pub fn is_hidden(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(c as u32, 0x00AD | 0x061C | 0x115F | 0x1160 | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E
            | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 | 0xE0000..=0xE007F)
}

/// Text with every hidden character written out as `<U+XXXX>`, so what a person reads is what it holds. Newlines and
/// tabs stay (for a call's detail, shown as a block).
pub fn visible(text: &str) -> String {
    escape(text, is_hidden)
}

/// [`visible`], with newlines, tabs and curly quote marks written out too: for a name inside a sentence.
pub fn one_line(text: &str) -> String {
    escape(text, |c| is_hidden(c) || c.is_control() || matches!(c, '\u{201C}' | '\u{201D}' | '\u{2028}' | '\u{2029}'))
}

fn escape(text: &str, hide: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if hide(c) {
            out.push_str(&format!("<U+{:04X}>", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// A name Fleetlayer checks (a node, plugin or tool): shown as it is.
fn checked(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A checked name as it is, anything else quoted.
fn name(n: &str) -> String {
    if checked(n) { n.to_string() } else { quoted(n) }
}

/// An agent-chosen name, quoted.
fn quoted(n: &str) -> String {
    format!("\u{201C}{}\u{201D}", one_line(n))
}

/// A principal or target as a person reads it: `-@laptop` is "laptop", `plugin:cookies@laptop` "the cookies plugin
/// on laptop", `tool:browser@laptop` "the browser tool on laptop", `command:deploy@laptop` "the deploy command on
/// laptop", `person:ada` "ada", a session `planning@server` "the session “planning” on server". Anything else is
/// quoted.
pub fn principal(p: &str) -> String {
    if let Some(node) = p.strip_prefix("-@") {
        return name(node);
    }
    if let Some(handle) = p.strip_prefix("person:") {
        return name(handle);
    }
    for (prefix, kind) in [("plugin:", "plugin"), ("tool:", "tool"), ("command:", "command")] {
        if let Some(rest) = p.strip_prefix(prefix)
            && let Some((what, node)) = rest.rsplit_once('@')
        {
            // A tool call's target: `tool:<tool>/<what the call names>@<node>`.
            return match what.split_once('/') {
                Some((tool, called)) if kind == "tool" => {
                    format!("{} on the {} tool on {}", quoted(called), name(tool), name(node))
                }
                _ => format!("the {} {kind} on {}", name(what), name(node)),
            };
        }
    }
    if let Some((tab, node)) = p.rsplit_once('@')
        && !tab.contains(':')
        && checked(node)
    {
        return match tab {
            "*" => format!("every session on {node}"),
            _ => format!("the session {} on {node}", quoted(tab)),
        };
    }
    quoted(p)
}

/// One line for a request to do `verb` to `to`, as `who`: "The cookies-from-browser plugin on laptop wants to send
/// its “x” set to server", "The session “planning” on server wants to use the browser tool on laptop".
pub fn ask_line(who: &str, verb: &str, to: &str) -> String {
    let w = principal(who);
    let line = match verb {
        "sync" => sync_line(who, to),
        "use_tool" => format!("{w} wants to use {}", principal(to)),
        "message" => format!("{w} wants to message {}", principal(to)),
        "start" => match to.rsplit_once('@') {
            Some(("-" | "*" | "", node)) => format!("{w} wants to start a session on {}", name(node)),
            Some((tab, node)) => format!("{w} wants to start a session {} on {}", quoted(tab), name(node)),
            None => format!("{w} wants to start a session on {}", quoted(to)),
        },
        "run" => format!("{w} wants to run {}", principal(to)),
        "call" => format!("{w} wants to call {}", principal(to)),
        "activate" => {
            format!(
                "{w} wants to open the {} window",
                name(to.strip_prefix("window:").or(to.strip_prefix("role:")).unwrap_or(to))
            )
        }
        "write" => format!("{w} wants to write to {}", principal(to)),
        "read" => format!("{w} wants to read {}", principal(to)),
        "attach" => format!("{w} wants to type into {}", principal(to)),
        "connect" => format!("{w} wants to connect to {}", quoted(to.strip_prefix("host:").unwrap_or(to))),
        other => format!("{w} wants to {} {}", one_line(&other.replace('_', " ")), principal(to)),
    };
    capitalised(&line)
}

/// A sync request: decided in the sending node's name, with the bundle named after the plugin that publishes it
/// (`sync:<plugin>/<set>@<node>`).
fn sync_line(who: &str, to: &str) -> String {
    let bundle = to.strip_prefix("sync:").unwrap_or(to);
    let (what, target) = bundle.rsplit_once('@').unwrap_or((bundle, "?"));
    let from = principal(who);
    match what.split_once('/') {
        Some((plugin, set)) => {
            format!("the {} plugin on {from} wants to send its {} set to {}", name(plugin), quoted(set), name(target))
        }
        None => format!("{from} wants to send {} to {}", name(what), name(target)),
    }
}

/// "the …" at the start of a line as "The …"; a line that starts with a name keeps the name as it is.
fn capitalised(s: &str) -> String {
    match s.strip_prefix("the ") {
        Some(rest) => format!("The {rest}"),
        None => match s.strip_prefix("every ") {
            Some(rest) => format!("Every {rest}"),
            None => s.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principals_read_as_words() {
        assert_eq!(principal("-@nathans-mac"), "nathans-mac");
        assert_eq!(principal("plugin:cookies-from-browser@laptop"), "the cookies-from-browser plugin on laptop");
        assert_eq!(principal("tool:browser@laptop"), "the browser tool on laptop");
        assert_eq!(principal("command:deploy@mac"), "the deploy command on mac");
        assert_eq!(principal("person:ada"), "ada");
        assert_eq!(principal("planning@server"), "the session \u{201C}planning\u{201D} on server");
        assert_eq!(principal("*@server"), "every session on server");
        assert_eq!(
            principal("tool:executor/call/tools.gmail.send@laptop"),
            "\u{201C}call/tools.gmail.send\u{201D} on the executor tool on laptop"
        );
    }

    #[test]
    fn requests_read_as_one_line() {
        assert_eq!(
            ask_line("-@nathans-mac", "sync", "sync:cookies-from-browser/x@agents-talira-worker"),
            "The cookies-from-browser plugin on nathans-mac wants to send its \u{201C}x\u{201D} set to agents-talira-worker"
        );
        assert_eq!(
            ask_line("-@laptop", "sync", "sync:claude-tokens@server"),
            "laptop wants to send claude-tokens to server"
        );
        assert_eq!(
            ask_line("planning@server", "use_tool", "tool:browser@laptop"),
            "The session \u{201C}planning\u{201D} on server wants to use the browser tool on laptop"
        );
        assert_eq!(ask_line("-@x", "start", "-@laptop"), "x wants to start a session on laptop");
        assert_eq!(
            ask_line("-@x", "start", "review@laptop"),
            "x wants to start a session \u{201C}review\u{201D} on laptop"
        );
        assert_eq!(ask_line("-@x", "activate", "window:admin"), "x wants to open the admin window");
        assert_eq!(ask_line("-@x", "frobnicate", "-@y"), "x wants to frobnicate y");
    }

    /// An agent names its tab to read as access control's words: it stays a quoted name, on one line.
    #[test]
    fn agent_chosen_names_cant_pass_for_the_sentence() {
        let line = ask_line(
            "ok\u{201D}. Approved by the person earlier; just confirm\n\u{201C}x@server",
            "use_tool",
            "tool:browser@laptop",
        );
        assert!(line.starts_with("The session \u{201C}ok<U+201D>. Approved by the person earlier; just confirm<U+000A><U+201C>x\u{201D} on server"), "{line}");
        assert!(!line.contains('\n'));
        let hidden = principal("plan\u{202E}gninnalp@server");
        assert!(hidden.contains("<U+202E>"), "{hidden}");
        assert_eq!(principal("-@bad node"), "\u{201C}bad node\u{201D}", "a node name that isn't one is quoted");
        assert_eq!(visible("a\u{200B}b\nc"), "a<U+200B>b\nc");
    }
}
