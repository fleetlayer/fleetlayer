//! Skills and the agent guide name `fleetlayer …` commands that agents run as written. This test checks every one
//! against the command line's own definition, so a renamed command or flag fails here, not in an agent's session.

use std::path::{Path, PathBuf};

use clap::CommandFactory;

/// The files agents read: every skill in the repo, and the guide whose block goes in a person's instructions.
fn agent_docs(root: &Path) -> Vec<PathBuf> {
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

/// Every `fleetlayer …` invocation in a line of code, as its words after `fleetlayer`.
fn invocations(line: &str) -> Vec<Vec<String>> {
    let w = words(line);
    let mut out = vec![];
    for (i, word) in w.iter().enumerate() {
        if word == "fleetlayer" {
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

/// What's wrong with `fleetlayer <args>`, if anything: an unknown subcommand or flag.
fn check(args: &[String]) -> Option<String> {
    let mut cmd = crate::Cli::command();
    cmd.build();
    let mut at = &cmd;
    let mut path = vec!["fleetlayer".to_string()];
    let mut walking = true;
    for a in args {
        if a.starts_with("--") {
            let long = a.trim_start_matches("--").split('=').next().unwrap_or("");
            if long.is_empty() {
                // `--` ends the flags.
                break;
            }
            let known = at
                .get_arguments()
                .any(|arg| arg.get_long() == Some(long) || arg.get_all_aliases().is_some_and(|al| al.contains(&long)));
            if !known {
                return Some(format!("`{}` has no flag --{long}", path.join(" ")));
            }
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

#[test]
fn every_command_agents_are_told_to_run_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut problems = vec![];
    let mut seen = 0;
    for file in agent_docs(&root) {
        let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        for (n, line) in code(&text) {
            for args in invocations(&line) {
                seen += 1;
                if let Some(p) = check(&args) {
                    let shown = file.strip_prefix(&root).unwrap_or(&file).display().to_string();
                    problems.push(format!("{shown}:{n}: {p}"));
                }
            }
        }
    }
    assert!(seen > 10, "found only {seen} fleetlayer commands in the skills: is the search broken?");
    assert!(problems.is_empty(), "skills name commands that don't exist:\n{}", problems.join("\n"));
}

#[test]
fn the_check_catches_what_it_should() {
    let w = |s: &str| invocations(s).remove(0);
    assert_eq!(check(&w("fleetlayer msg send --to a@b \"hi there --nope\"")), None);
    assert_eq!(check(&w("claude mcp add browser -- fleetlayer mcp browser@laptop --why x")), None);
    assert_eq!(check(&w("fleetlayer summaries deploy rollback --long")), None);
    assert_eq!(check(&w("fleetlayer --json status")), None);
    assert!(check(&w("fleetlayer --json stauts")).is_some());
    assert_eq!(check(&w("fleetlayer session start --tab r --cwd /x   # a comment --bogus")), None);
    assert!(check(&w("fleetlayer msg post --to a")).unwrap().contains("no subcommand post"));
    assert!(check(&w("fleetlayer session start --tabname r")).unwrap().contains("no flag --tabname"));
    assert!(check(&w("fleetlayer stauts")).unwrap().contains("no subcommand stauts"));
    assert!(invocations("fleetlayer-node install").is_empty());
}
