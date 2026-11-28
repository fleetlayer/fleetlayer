//! `fleetlayer presence`: whether the person is at their desk, on their phone, away or asleep, from the fleet's
//! presence authority (`docs/plugins/presence.md`), a tool its node shares. Each command opens the tool through this
//! node's tools socket, as `fleetlayer lease` does, so the control plane checks and audits every call.

use std::path::Path;

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::lease::{call_tool, find_tool};

#[derive(Args)]
pub struct PresenceArgs {
    #[command(subcommand)]
    command: Option<PresenceCommand>,
    #[command(flatten)]
    who: Who,
    /// With no command: exit 1 unless the state is one of these (present, semi_present, away, asleep), for scripts:
    /// `fleetlayer presence --is present,semi_present || exit 0`.
    #[arg(long, value_delimiter = ',')]
    is: Vec<String>,
}

#[derive(Args, Clone)]
struct Who {
    /// The person (their handle). Default $FLEETLAYER_PERSON, else the only person set up.
    #[arg(long, env = "FLEETLAYER_PERSON", global = true)]
    person: Option<String>,
    /// The presence authority: `<node>` or `<tool>@<node>`. Default $FLEETLAYER_PRESENCE, else the one `presence`
    /// tool this session may use.
    #[arg(long, env = "FLEETLAYER_PRESENCE", global = true)]
    at: Option<String>,
}

#[derive(Subcommand)]
enum PresenceCommand {
    /// The person's state, since when and why (the default).
    Show {
        /// Exit 1 unless the state is one of these (present, semi_present, away, asleep).
        #[arg(long, value_delimiter = ',')]
        is: Vec<String>,
    },
    /// Record what the person told you: `away --for 1h --note lunch`, `asleep`, or `back`. Only when they said so.
    Set {
        /// away, asleep or back.
        state: String,
        /// How long: 30m, 1h, 2h30m.
        #[arg(long = "for")]
        for_: Option<String>,
        /// Until when, their time: 14:00, or 2026-12-20 14:00.
        #[arg(long)]
        until: Option<String>,
        /// What they said, in a few words: lunch, school run.
        #[arg(long)]
        note: Option<String>,
    },
    /// Planned absences (holidays): `plan away --from 2026-12-20 --to 2027-01-05 --note holiday`, `plan list`,
    /// `plan remove <id>`.
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Say the person just looked at a page (for a service that shows them pages: a deck, a relay).
    Seen {
        /// What they looked at: quietdeck.
        #[arg(long)]
        via: String,
        /// phone, desk or other.
        #[arg(long, default_value = "other")]
        device: String,
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    /// Add a planned absence. Times are the person's own; a bare date for --to includes that day.
    Away {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// The person's planned absences, with their numbers.
    List,
    /// Remove a planned absence by its number.
    Remove { id: u64 },
}

const CLIENT: &str = "fleetlayer presence";

pub async fn run(socket: &Path, args: PresenceArgs, as_json: bool) -> anyhow::Result<()> {
    let who = args.who.clone();
    let target = find_tool(socket, who.at.as_deref(), "presence", "presence authority", "FLEETLAYER_PRESENCE").await?;
    let call = |name: &'static str, a: Value| {
        let target = target.clone();
        async move { call_tool(socket, &target, CLIENT, name, a).await }
    };
    // Changes name the person, so the access policy can decide who may change whose status.
    let person = || async {
        if let Some(p) = &who.person {
            return Ok::<_, anyhow::Error>(p.clone());
        }
        let v = call("get", json!({})).await?;
        match v["people"].as_array().map(Vec::as_slice) {
            Some([one]) => Ok(one["person"].as_str().unwrap_or_default().to_string()),
            _ => anyhow::bail!("several people are set up: name one with --person"),
        }
    };
    match args.command.unwrap_or(PresenceCommand::Show { is: args.is }) {
        PresenceCommand::Show { is } => {
            let v = call("get", json!({"person": who.person})).await?;
            let people = v["people"].as_array().cloned().unwrap_or_default();
            if as_json {
                let out = if who.person.is_some() || people.len() == 1 { people[0].clone() } else { v.clone() };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for p in &people {
                    println!("{}", p["line"].as_str().unwrap_or(""));
                }
            }
            if !is.is_empty() {
                let state = people.first().and_then(|p| p["state"].as_str()).unwrap_or("");
                let wanted: Vec<String> = is.iter().map(|s| s.replace('-', "_")).collect();
                if !wanted.iter().any(|w| w == state) {
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        PresenceCommand::Set { state, for_, until, note } => {
            let p = person().await?;
            let v =
                call("set", json!({"person": p, "state": state, "for": for_, "until": until, "note": note})).await?;
            done(&v, as_json)
        }
        PresenceCommand::Plan { command: PlanCommand::Away { from, to, note } } => {
            let p = person().await?;
            let v = call("plan_add", json!({"person": p, "from": from, "to": to, "note": note})).await?;
            if !as_json {
                eprintln!("Planned absence #{} added.", v["plan"]["id"]);
            }
            done(&v, as_json)
        }
        PresenceCommand::Plan { command: PlanCommand::List } => {
            let v = call("get", json!({"person": who.person})).await?;
            let people = v["people"].as_array().cloned().unwrap_or_default();
            if as_json {
                let plans: Vec<Value> =
                    people.iter().map(|p| json!({"person": p["person"], "plans": p["plans"]})).collect();
                println!("{}", serde_json::to_string_pretty(&plans)?);
                return Ok(());
            }
            for p in &people {
                let plans = p["plans"].as_array().cloned().unwrap_or_default();
                if plans.is_empty() {
                    println!("{}: no planned absences", p["person"].as_str().unwrap_or("?"));
                }
                for x in plans {
                    println!(
                        "{} #{}: {} to {}{} (added by {})",
                        p["person"].as_str().unwrap_or("?"),
                        x["id"],
                        x["from"].as_str().unwrap_or("?"),
                        x["to"].as_str().unwrap_or("?"),
                        x["note"].as_str().map(|n| format!(", {n}")).unwrap_or_default(),
                        x["by"].as_str().unwrap_or("?")
                    );
                }
            }
            Ok(())
        }
        PresenceCommand::Plan { command: PlanCommand::Remove { id } } => {
            let p = person().await?;
            let v = call("plan_remove", json!({"person": p, "id": id})).await?;
            done(&v, as_json)
        }
        PresenceCommand::Seen { via, device } => {
            let p = person().await?;
            let v = call("seen", json!({"person": p, "via": via, "device": device})).await?;
            done(&v, as_json)
        }
    }
}

/// After a change: the person's state now.
fn done(v: &Value, as_json: bool) -> anyhow::Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(v)?);
    } else {
        println!("{}", v["now"]["line"].as_str().unwrap_or(""));
    }
    Ok(())
}
