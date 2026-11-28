//! Checking the commands that skills tell agents to run against a command line's clap definition. Shared, by
//! `#[path]`, with the other binaries whose commands skills name (`fleetlayer-control`), each in its own test.

use std::path::{Path, PathBuf};

/// The files agents read: every skill in the repo, and the guide whose block goes in a person's instructions.
pub fn agent_docs(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    walk(root, &mut |p| {
        if p.file_name().is_some_and(|n| n == "SKILL.md") {
            out.push(p.to_path_buf());
        }
    });
    let guide = root.join("docs/guides/agents-know-fleetlayer.md");
    if guide.exists() {
        out.push(guide);
    }
    out.sort();
    out
}

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if p.is_dir() {
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                walk(&p, f);
            }
        } else {
            f(&p);
        }
    }
}

/// The code in a Markdown file, line by line: lines of fenced blocks, and each `inline span`.
fn code(text: &str) -> Vec<(usize, String)> {
    let mut out = vec![];
    let mut fenced = false;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            out.push((n, line.to_string()));
            continue;
        }
        let mut parts = line.split('`');
        parts.next();
        while let Some(span) = parts.next() {
            out.push((n, span.to_string()));
            parts.next();
        }
    }
    out
}

/// Words as a shell splits them, roughly: quotes group, and a comment ends the line.
fn words(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c == '#' && !started => break,
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// Every `<program> …` invocation in a line of code, as its words after the program's name.
pub fn invocations(line: &str, program: &str) -> Vec<Vec<String>> {
    let w = words(line);
    let mut out = vec![];
    for (i, word) in w.iter().enumerate() {
        if word == program {
            let rest: Vec<String> = w[i + 1..]
                .iter()
                .take_while(|t| !matches!(t.as_str(), "|" | "||" | "&&" | ";" | ">" | "2>&1"))
                .cloned()
                .collect();
            if !rest.is_empty() {
                out.push(rest);
            }
        }
    }
    out
}

/// What's wrong with `<cmd> <args>`, if anything: an unknown subcommand or flag.
pub fn check(cmd: &clap::Command, args: &[String]) -> Option<String> {
    let mut cmd = cmd.clone();
    cmd.build();
    let mut at = &cmd;
    let mut path = vec![cmd.get_name().to_string()];
    let mut walking = true;
    let mut value_next = false;
    for a in args {
        if std::mem::take(&mut value_next) && !a.starts_with('-') {
            // The value of the flag before it (`--tenant acme`).
            continue;
        }
        if a.starts_with("--") {
            // Past its positionals, a command that passes the rest through (`fleetlayer run <target> --rack x`) takes
            // any flag as its own argument.
            if !walking && at.get_positionals().any(|p| p.is_trailing_var_arg_set()) {
                break;
            }
            let long = a.trim_start_matches("--").split('=').next().unwrap_or("");
            if long.is_empty() {
                // `--` ends the flags.
                break;
            }
            let known = at
                .get_arguments()
                .find(|arg| arg.get_long() == Some(long) || arg.get_all_aliases().is_some_and(|al| al.contains(&long)));
            let Some(known) = known else {
                return Some(format!("`{}` has no flag --{long}", path.join(" ")));
            };
            value_next = !a.contains('=') && known.get_action().takes_values();
            continue;
        }
        if !walking || a.starts_with('-') {
            continue;
        }
        if at.has_subcommands() {
            match at.find_subcommand(a) {
                Some(sub) => {
                    at = sub;
                    path.push(a.clone());
                }
                None => return Some(format!("`{}` has no subcommand {a}", path.join(" "))),
            }
        } else {
            walking = false;
        }
    }
    None
}

/// Every `<program> …` command in the files agents read under `root`, checked against `cmd`: how many there were, and
/// what's wrong with them.
pub fn problems(root: &Path, program: &str, cmd: &clap::Command) -> (usize, Vec<String>) {
    let mut problems = vec![];
    let mut seen = 0;
    for file in agent_docs(root) {
        let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        for (n, line) in code(&text) {
            for args in invocations(&line, program) {
                seen += 1;
                if let Some(p) = check(cmd, &args) {
                    let shown = file.strip_prefix(root).unwrap_or(&file).display().to_string();
                    problems.push(format!("{shown}:{n}: {p}"));
                }
            }
        }
    }
    (seen, problems)
}
