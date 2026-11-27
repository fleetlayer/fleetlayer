//! Skills and the agent guide name `fleetlayer …` commands that agents run as written. This test checks every one
//! against the command line's own definition, so a renamed command or flag fails here, not in an agent's session.

use std::path::Path;

use clap::CommandFactory;

use crate::skill_check::{check as check_in, invocations, problems};

fn check(args: &[String]) -> Option<String> {
    check_in(&crate::Cli::command(), args)
}

#[test]
fn every_command_agents_are_told_to_run_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (seen, problems) = problems(&root, "fleetlayer", &crate::Cli::command());
    assert!(seen > 10, "found only {seen} fleetlayer commands in the skills: is the search broken?");
    assert!(problems.is_empty(), "skills name commands that don't exist:\n{}", problems.join("\n"));
}

#[test]
fn the_check_catches_what_it_should() {
    let w = |s: &str| invocations(s, "fleetlayer").remove(0);
    assert_eq!(check(&w("fleetlayer msg send --to a@b \"hi there --nope\"")), None);
    assert_eq!(check(&w("claude mcp add browser -- fleetlayer mcp browser@laptop --why x")), None);
    assert_eq!(check(&w("fleetlayer summaries deploy rollback --long")), None);
    assert_eq!(check(&w("fleetlayer --json status")), None);
    assert!(check(&w("fleetlayer --json stauts")).is_some());
    assert_eq!(check(&w("fleetlayer session start --tab r --cwd /x   # a comment --bogus")), None);
    assert!(check(&w("fleetlayer msg post --to a")).unwrap().contains("no subcommand post"));
    assert!(check(&w("fleetlayer session start --tabname r")).unwrap().contains("no flag --tabname"));
    assert!(check(&w("fleetlayer stauts")).unwrap().contains("no subcommand stauts"));
    assert!(invocations("fleetlayer-node install", "fleetlayer").is_empty());
}
