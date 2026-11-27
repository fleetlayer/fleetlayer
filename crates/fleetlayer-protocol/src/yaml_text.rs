//! Small edits to a person's YAML settings file as text, keeping their comments and layout: Fleetlayer changes one
//! value (a moved control plane's address) or one block (a plugin moving to another node), never re-serialises the
//! file. Each edit answers None when the file isn't laid out the way it expects (flow style, say), so the caller can
//! say what to change by hand.

/// A string as a YAML scalar (JSON's quoting is valid YAML).
pub fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("a string serialises")
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_content(line: &str) -> bool {
    let t = line.trim_start();
    !t.is_empty() && !t.starts_with('#')
}

/// The lines (start, end exclusive) of top-level key `section`'s block, its own line included.
pub fn section(text: &str, key: &str) -> Option<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|l| indent(l) == 0 && l.split_once(':').is_some_and(|(k, _)| k.trim() == key))?;
    let mut end = start + 1;
    while end < lines.len() && (!is_content(lines[end]) || indent(lines[end]) > 0) {
        end += 1;
    }
    // Trailing blank and comment lines belong to what follows.
    while end > start + 1 && !is_content(lines[end - 1]) {
        end -= 1;
    }
    Some((start, end))
}

fn join(lines: &[String], trailing_newline: bool) -> String {
    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    out
}

/// Set `key: value` inside the top-level block `section` (`control:` → `url:`), keeping the line's indentation.
pub fn set_in_section(text: &str, section_key: &str, key: &str, value: &str) -> Option<String> {
    let (start, end) = section(text, section_key)?;
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let i = (start + 1..end).find(|&i| {
        let t = lines[i].trim_start();
        t.split_once(':').is_some_and(|(k, _)| k.trim() == key)
    })?;
    let pad = " ".repeat(indent(&lines[i]));
    lines[i] = format!("{pad}{key}: {}", quote(value));
    Some(join(&lines, text.ends_with('\n')))
}

/// Set a top-level `key: value`, adding it at the end when it isn't there.
pub fn set_top(text: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let line = format!("{key}: {}", quote(value));
    match lines.iter().position(|l| indent(l) == 0 && l.split_once(':').is_some_and(|(k, _)| k.trim() == key)) {
        Some(i) => lines[i] = line,
        None => lines.push(line),
    }
    join(&lines, true)
}

/// The lines (start, end exclusive) of the `plugins:` list item called `name`, comments directly above it included.
pub fn plugin_block(text: &str, name: &str) -> Option<(usize, usize)> {
    let (start, end) = section(text, "plugins")?;
    let lines: Vec<&str> = text.lines().collect();
    let item_starts: Vec<usize> = (start + 1..end).filter(|&i| lines[i].trim_start().starts_with("- ")).collect();
    let item_indent = item_starts.first().map(|&i| indent(lines[i]))?;
    let item_starts: Vec<usize> = item_starts.into_iter().filter(|&i| indent(lines[i]) == item_indent).collect();
    for (n, &s) in item_starts.iter().enumerate() {
        let e = item_starts.get(n + 1).copied().unwrap_or(end);
        let named = (s..e).any(|i| {
            let t = lines[i].trim_start().trim_start_matches("- ");
            let field_indent = if i == s { item_indent + 2 } else { indent(lines[i]) };
            field_indent == item_indent + 2
                && t.split_once(':').is_some_and(|(k, v)| {
                    k.trim() == "name"
                        && v.split('#').next().unwrap_or("").trim().trim_matches('"').trim_matches('\'') == name
                })
        });
        if named {
            // Comment lines just above belong to the item; so does its end, up to the next item, without trailing
            // blank lines.
            let mut from = s;
            while from > start + 1 && lines[from - 1].trim_start().starts_with('#') {
                from -= 1;
            }
            let mut to = e;
            while to > s + 1 && !is_content(lines[to - 1]) {
                to -= 1;
            }
            return Some((from, to));
        }
    }
    None
}

/// The plugin item's text (its lines, each ending in a newline).
pub fn plugin_text(text: &str, name: &str) -> Option<String> {
    let (s, e) = plugin_block(text, name)?;
    Some(text.lines().skip(s).take(e - s).map(|l| format!("{l}\n")).collect())
}

/// Comment the plugin item out, with a first line saying why: it stays in the file to read, and nothing runs it.
pub fn comment_out_plugin(text: &str, name: &str, why: &str) -> Option<String> {
    let (s, e) = plugin_block(text, name)?;
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let pad = " ".repeat(lines[s..e].iter().filter(|l| is_content(l)).map(|l| indent(l)).min().unwrap_or(2));
    for l in &mut lines[s..e] {
        if is_content(l) {
            *l = format!("{pad}# {}", &l[pad.len().min(indent(l))..]);
        }
    }
    lines.insert(s, format!("{pad}# {why}"));
    Some(join(&lines, text.ends_with('\n')))
}

/// Add a plugin item (text as [`plugin_text`] gives it) at the end of `plugins:`, adding the section when there's none.
/// None when a plugin of that name is already there.
pub fn add_plugin(text: &str, name: &str, item: &str) -> Option<String> {
    if plugin_block(text, name).is_some() {
        return None;
    }
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let item_lines = item.lines().map(String::from);
    match section(text, "plugins") {
        // An empty list written inline becomes a block.
        Some((s, e)) if lines[s].split('#').next().unwrap_or("").trim_end().ends_with("[]") => {
            lines[s] = "plugins:".into();
            lines.splice(e..e, item_lines);
        }
        // Another inline value: can't append to it as text.
        Some((s, _)) if !lines[s].trim_end().ends_with(':') => return None,
        Some((_, e)) => {
            lines.splice(e..e, item_lines);
        }
        None => {
            lines.push("plugins:".into());
            lines.extend(item_lines);
        }
    }
    Some(join(&lines, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: &str = "# mine\nname: laptop\ncontrol:\n  url: \"ws://127.0.0.1:7600/v1/link\"   \n  token_file: /h/.config/fleetlayer/node-token\ngateway: \"http://127.0.0.1:7680\"\nplugins:\n  # The hub.\n  - name: claude-auth-manager\n    command: fleetlayer-claude-auth-manager\n    config:\n      role: hub\n      hub:\n        listen: 127.0.0.1:0\n\n  # Pages.\n  - name: pages\n    command: fleetlayer-pages\n    config: { listen: 127.0.0.1:0 }\n# harnesses:\n#   claude: {}\n";

    #[test]
    fn a_value_in_a_section_changes_and_nothing_else() {
        let out = set_in_section(NODE, "control", "url", "wss://control.example/v1/link").unwrap();
        assert!(out.contains("  url: \"wss://control.example/v1/link\"\n  token_file:"));
        assert_eq!(out.lines().count(), NODE.lines().count());
        assert!(out.starts_with("# mine\n") && out.ends_with("#   claude: {}\n"));
        assert_eq!(set_in_section("control: { url: x }\n", "control", "url", "y"), None, "flow style: by hand");
        assert_eq!(set_in_section(NODE, "heartbeat", "url", "y"), None);
        let t = set_top(NODE, "gateway", "http://10.0.0.5:7680");
        assert!(t.contains("\ngateway: \"http://10.0.0.5:7680\"\n"));
        assert!(set_top("name: a\n", "gateway", "g").ends_with("gateway: \"g\"\n"));
    }

    #[test]
    fn a_plugin_block_is_found_moved_and_commented_out() {
        let hub = plugin_text(NODE, "claude-auth-manager").unwrap();
        assert!(hub.starts_with("  # The hub.\n  - name: claude-auth-manager\n"));
        assert!(hub.ends_with("        listen: 127.0.0.1:0\n"), "{hub:?}");
        let pages = plugin_text(NODE, "pages").unwrap();
        assert_eq!(
            pages,
            "  # Pages.\n  - name: pages\n    command: fleetlayer-pages\n    config: { listen: 127.0.0.1:0 }\n"
        );
        assert_eq!(plugin_text(NODE, "herdr"), None);

        let out = comment_out_plugin(NODE, "pages", "Moved to server by fleetlayer move.").unwrap();
        let v: serde_json::Value = yaml_serde::from_str(&out).unwrap();
        assert_eq!(v["plugins"].as_array().unwrap().len(), 1, "{out}");
        assert!(out.contains("  # Moved to server by fleetlayer move.\n  # Pages.\n  # - name: pages\n  #   command: fleetlayer-pages\n"), "{out}");

        let dest =
            "name: server\nplugins:\n  - name: herdr\n    command: fleetlayer-herdr\nheartbeat:\n  url: http://x\n";
        let added = add_plugin(dest, "pages", &pages).unwrap();
        let v: serde_json::Value = yaml_serde::from_str(&added).unwrap();
        assert_eq!(v["plugins"][1]["name"], "pages");
        assert_eq!(v["heartbeat"]["url"], "http://x");
        assert_eq!(add_plugin(&added, "pages", &pages), None, "never twice");
        let v: serde_json::Value = yaml_serde::from_str(&add_plugin("name: s\n", "pages", &pages).unwrap()).unwrap();
        assert_eq!(v["plugins"][0]["config"]["listen"], "127.0.0.1:0");
        let v: serde_json::Value =
            yaml_serde::from_str(&add_plugin("plugins: []\nname: s\n", "pages", &pages).unwrap()).unwrap();
        assert_eq!(v["plugins"][0]["name"], "pages");
        assert_eq!(v["name"], "s");
        assert_eq!(add_plugin("plugins: [{name: x, command: y}]\n", "pages", &pages), None);
    }
}
