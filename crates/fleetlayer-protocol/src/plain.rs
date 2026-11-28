//! Access control's principals and requests in plain words, for people: what an approval prompt, a list of requests or
//! a dashboard shows first. The raw forms (`-@laptop`, `plugin:cookies@laptop`, `sync:cookies-from-browser/x@server`)
//! stay what rules are written in and what's stored; these sentences are built only from them, never from an agent's
//! own text, so they're access control's words.

/// A principal or target as a person reads it: `-@laptop` is "laptop", `plugin:cookies@laptop` "the cookies plugin
/// on laptop", `tool:browser@laptop` "the browser tool on laptop", `command:deploy@laptop` "the deploy command on
/// laptop", `person:ada` "ada". Anything else (a session's `tab@node`, a pattern) is shown as it is.
pub fn principal(p: &str) -> String {
    if let Some(node) = p.strip_prefix("-@") {
        return node.to_string();
    }
    if let Some(handle) = p.strip_prefix("person:") {
        return handle.to_string();
    }
    for (prefix, kind) in [("plugin:", "plugin"), ("tool:", "tool"), ("command:", "command")] {
        if let Some(rest) = p.strip_prefix(prefix)
            && let Some((name, node)) = rest.rsplit_once('@')
        {
            return format!("the {name} {kind} on {node}");
        }
    }
    p.to_string()
}

/// One line for a request to do `verb` to `to`, as `who`: "The cookies-from-browser plugin on laptop wants to send
/// its x set to server", "planning@server wants to use the browser tool on laptop".
pub fn ask_line(who: &str, verb: &str, to: &str) -> String {
    let w = principal(who);
    let line = match verb {
        "sync" => sync_line(who, to),
        "use_tool" => format!("{w} wants to use {}", principal(to)),
        "message" => format!("{w} wants to message {}", principal(to)),
        "start" => format!("{w} wants to start a session on {}", node_of(to)),
        "run" => format!("{w} wants to run {}", principal(to)),
        "call" => format!("{w} wants to call {}", principal(to)),
        "activate" => format!(
            "{w} wants to open the {} window",
            to.strip_prefix("window:").or_else(|| to.strip_prefix("role:")).unwrap_or(to)
        ),
        "write" => format!("{w} wants to write to {}", principal(to)),
        "read" => format!("{w} wants to read {}", principal(to)),
        "attach" => format!("{w} wants to type into {}", principal(to)),
        "connect" => format!("{w} wants to connect to {}", to.strip_prefix("host:").unwrap_or(to)),
        other => format!("{w} wants to {} {}", other.replace('_', " "), principal(to)),
    };
    capitalised(&line)
}

/// A sync request: decided in the sending node's name, with the bundle named after the plugin that publishes it
/// (`sync:<plugin>/<set>@<node>`).
fn sync_line(who: &str, to: &str) -> String {
    let bundle = to.strip_prefix("sync:").unwrap_or(to);
    let (name, target) = bundle.rsplit_once('@').unwrap_or((bundle, "?"));
    let from = principal(who);
    match name.split_once('/') {
        Some((plugin, set)) => format!("the {plugin} plugin on {from} wants to send its {set} set to {target}"),
        None => format!("{from} wants to send {name} to {target}"),
    }
}

/// `-@laptop` or `*@laptop` as "laptop".
fn node_of(t: &str) -> String {
    t.strip_prefix("-@").or_else(|| t.strip_prefix("*@")).unwrap_or(t).to_string()
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
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
        assert_eq!(principal("planning@server"), "planning@server");
        assert_eq!(principal("*@server"), "*@server");
    }

    #[test]
    fn requests_read_as_one_line() {
        assert_eq!(
            ask_line("-@nathans-mac", "sync", "sync:cookies-from-browser/x@agents-talira-worker"),
            "The cookies-from-browser plugin on nathans-mac wants to send its x set to agents-talira-worker"
        );
        assert_eq!(
            ask_line("-@laptop", "sync", "sync:claude-tokens@server"),
            "Laptop wants to send claude-tokens to server"
        );
        assert_eq!(
            ask_line("planning@server", "use_tool", "tool:browser@laptop"),
            "Planning@server wants to use the browser tool on laptop"
        );
        assert_eq!(ask_line("a@x", "start", "-@laptop"), "A@x wants to start a session on laptop");
        assert_eq!(ask_line("a@x", "activate", "window:admin"), "A@x wants to open the admin window");
        assert_eq!(ask_line("a@x", "frobnicate", "-@y"), "A@x wants to frobnicate y");
    }
}
