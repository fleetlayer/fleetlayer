//! Acting on a running session for a caller: closing it, or answering the prompt it's blocked on
//! (`fleetlayer session close|answer`, `docs/protocol.md`, "Answering a blocked prompt"). Here, or on another node
//! through the control plane, checked there as `close` or `answer` on `<session>@<node>`.
//!
//! An answer is typed into the session's own menu through its session manager (keys, and text for a free answer),
//! and only after reading the screen shows that same prompt: a prompt that's been answered or replaced is refused
//! with `-32015`, so a late click never answers the wrong question.

use std::time::Duration;

use fleetlayer_protocol::ext::{self, BlockedPrompt, KeyStep, PromptAnswer, PromptKind, PromptOption};
use fleetlayer_protocol::link::{self as l, SessionActRequest, SessionActServe, SessionActed, SessionAction};
use fleetlayer_protocol::rpc::{RpcError, code};
use serde_json::{Value, json};

use crate::Node;
use crate::sessions::{Change, Session};

/// A local caller's close or answer, here or on another node.
pub async fn act(
    node: &Node,
    to: &str,
    action: SessionAction,
    why: Option<String>,
    caller: String,
) -> Result<SessionActed, RpcError> {
    let (name, target) = crate::deliver::parse_address(to);
    match target {
        Some(t) if t != node.name => {
            let peer = node.link.peer().await.ok_or_else(|| {
                RpcError::not_here(format!(
                    "{to} is on another node, and {} isn't linked to a control plane now",
                    node.name
                ))
            })?;
            let method = match action {
                SessionAction::Close => l::method::SESSION_CLOSE,
                SessionAction::Answer { .. } => l::method::SESSION_ANSWER,
            };
            let req = SessionActRequest { node: t.into(), session: name.into(), action, caller, justification: why };
            peer.request_within(method, &req, Duration::from_secs(40)).await.map_err(|e| {
                if e.code == code::METHOD_NOT_FOUND {
                    RpcError::refused(format!(
                        "the control plane is older than this node and can't {} sessions on other nodes: update it",
                        req.action.verb()
                    ))
                } else {
                    e
                }
            })
        }
        _ => here(node, name, action, &caller).await,
    }
}

/// Control plane → node: a close or answer for a caller on another node, already allowed there.
pub async fn serve(node: &Node, req: SessionActServe) -> Result<Value, RpcError> {
    let done = here(node, &req.session, req.action, &req.caller).await?;
    serde_json::to_value(done).map_err(|e| RpcError::internal(e.to_string()))
}

pub(crate) async fn here(
    node: &Node,
    name: &str,
    action: SessionAction,
    caller: &str,
) -> Result<SessionActed, RpcError> {
    let session = crate::deliver::resolve(node, name).await?;
    let manager = session
        .manager
        .clone()
        .ok_or_else(|| RpcError::refused(format!("{name} on {} has no session manager", node.name)))?;
    let plugin =
        node.plugin(&manager).ok_or_else(|| RpcError::refused(format!("plugin {manager} isn't configured")))?;
    let peer = plugin.peer().await.ok_or_else(|| RpcError::refused(format!("plugin {manager} isn't running")))?;
    let done = match action {
        SessionAction::Close => {
            let r = json!({"sessionId": session.session_id});
            peer.request_within::<_, Value>(
                fleetlayer_protocol::acp::method::SESSION_CLOSE,
                &r,
                Duration::from_secs(15),
            )
            .await?;
            tracing::info!(session = %session.session_id, tab = ?session.tab(), %caller, "closed a session");
            "closed"
        }
        SessionAction::Answer { prompt, answers } => {
            if !plugin.roles().await.session_manager.is_some_and(|r| r.keys) {
                return Err(RpcError::refused(format!("{manager} can't type into its sessions")));
            }
            answer(&peer, &session, &prompt, &answers).await?;
            tracing::info!(
                session = %session.session_id, tab = ?session.tab(), %caller, %prompt,
                answers = %serde_json::to_string(&answers).unwrap_or_default(), "answered a prompt"
            );
            "answered"
        }
    };
    Ok(SessionActed {
        node: node.name.clone(),
        session_id: session.session_id.clone(),
        tab: session.tab().map(str::to_string),
        done: done.into(),
    })
}

fn gone() -> RpcError {
    RpcError::new(code::PROMPT_GONE, "that prompt isn't showing any more (answered or gone)")
}

async fn screen(peer: &fleetlayer_plugin::Peer, session_id: &str) -> Result<String, RpcError> {
    let v: Value = peer
        .request_within(
            ext::method::SESSION_SCREEN,
            &json!({"sessionId": session_id, "lines": 80}),
            Duration::from_secs(8),
        )
        .await?;
    Ok(v["text"].as_str().unwrap_or("").to_string())
}

async fn answer(
    peer: &fleetlayer_plugin::Peer,
    session: &Session,
    prompt_id: &str,
    answers: &[PromptAnswer],
) -> Result<(), RpcError> {
    let prompt = session.meta.blocked_prompt().filter(|p| p.id == prompt_id).ok_or_else(gone)?.clone();
    let shown = screen(peer, &session.session_id).await?;
    let menu = parse_menu(&shown);
    if !showing(&prompt, &shown, &menu) {
        return Err(gone());
    }
    let steps = keys_for(&prompt, &menu, answers)?;
    let req = ext::SessionKeys { session_id: session.session_id.clone(), steps };
    peer.request_within::<_, Value>(ext::method::SESSION_KEYS, &req, Duration::from_secs(20)).await?;
    // A form with several questions, or a multiple choice, ends on a review: submit it once it shows.
    if needs_review(&prompt) {
        tokio::time::sleep(Duration::from_millis(900)).await;
        let review = screen(peer, &session.session_id).await?;
        if !review.contains("Submit answers") {
            return Err(RpcError::refused(
                "the answers went in, but the form's review step didn't show: look at the session before answering again",
            ));
        }
        let submit = ext::SessionKeys { session_id: session.session_id.clone(), steps: vec![KeyStep::Key("1".into())] };
        peer.request_within::<_, Value>(ext::method::SESSION_KEYS, &submit, Duration::from_secs(10)).await?;
    }
    Ok(())
}

/// Whether the screen shows this prompt now: a question form showing its first question, or the permission or
/// plan menu with the same options as when it was reported.
fn showing(p: &BlockedPrompt, screen: &str, menu: &[PromptOption]) -> bool {
    match p.kind {
        PromptKind::Question => p.questions.first().is_some_and(|q| {
            screen.contains(q.question.trim()) && q.options.first().is_none_or(|o| screen.contains(o.label.trim()))
        }),
        PromptKind::Permission | PromptKind::Plan => {
            !menu.is_empty() && (p.options.is_empty() || same_labels(&p.options, menu))
        }
    }
}

fn same_labels(a: &[PromptOption], b: &[PromptOption]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.n == y.n && x.label == y.label)
}

fn needs_review(p: &BlockedPrompt) -> bool {
    p.kind == PromptKind::Question && (p.questions.len() > 1 || p.questions.iter().any(|q| q.multi_select))
}

fn digit(n: u32) -> KeyStep {
    KeyStep::Key(n.to_string())
}

/// The keys that give these answers in Claude Code's menus.
fn keys_for(p: &BlockedPrompt, menu: &[PromptOption], answers: &[PromptAnswer]) -> Result<Vec<KeyStep>, RpcError> {
    let bad = |m: String| Err(RpcError::invalid_params(m));
    let one = |a: &PromptAnswer| a.pick.or_else(|| (a.picks.len() == 1).then(|| a.picks[0]));
    match p.kind {
        PromptKind::Question => {
            if answers.len() != p.questions.len() {
                return bad(format!(
                    "this form has {} question(s): give one answer for each, in order",
                    p.questions.len()
                ));
            }
            let mut steps = vec![];
            for (i, (q, a)) in p.questions.iter().zip(answers).enumerate() {
                let label = format!("question {} ({})", i + 1, q.header);
                let valid = |n: u32| (1..=q.options.len() as u32).contains(&n);
                if let Some(text) = &a.text {
                    if q.multi_select {
                        return bad(format!("{label} is a multiple choice: pick options, not text"));
                    }
                    let other = q.other.ok_or_else(|| RpcError::invalid_params(format!("{label} takes no text")))?;
                    steps.extend([digit(other), KeyStep::Text(text.clone()), KeyStep::Key("enter".into())]);
                } else if q.multi_select {
                    let picks: Vec<u32> =
                        if a.picks.is_empty() { a.pick.into_iter().collect() } else { a.picks.clone() };
                    if picks.is_empty() || !picks.iter().all(|n| valid(*n)) {
                        return bad(format!("{label}: pick options 1 to {}", q.options.len()));
                    }
                    steps.extend(picks.into_iter().map(digit));
                    steps.push(KeyStep::Key("tab".into()));
                } else {
                    match one(a) {
                        Some(n) if valid(n) => steps.push(digit(n)),
                        _ => return bad(format!("{label}: pick one option, 1 to {}", q.options.len())),
                    }
                }
            }
            Ok(steps)
        }
        PromptKind::Permission | PromptKind::Plan => {
            let [a] = answers else { return bad("give one answer".into()) };
            if let Some(text) = &a.text {
                if p.kind == PromptKind::Permission {
                    return bad("a permission prompt takes a pick; send instructions as a message afterwards".into());
                }
                let o = menu
                    .iter()
                    .find(|o| o.label.starts_with("Tell Claude"))
                    .ok_or_else(|| RpcError::invalid_params("this menu has no option for written feedback"))?;
                return Ok(vec![digit(o.n), KeyStep::Text(text.clone()), KeyStep::Key("enter".into())]);
            }
            match one(a) {
                Some(n) if menu.iter().any(|o| o.n == n) => Ok(vec![digit(n)]),
                _ => bad(format!("pick one of the menu's options, 1 to {}", menu.len())),
            }
        }
    }
}

/// The numbered menu around the cursor (`❯ 1. Yes`) on a Claude Code screen: the options in order, without their
/// description lines. Empty when no menu shows.
pub fn parse_menu(screen: &str) -> Vec<PromptOption> {
    let lines: Vec<&str> = screen.lines().collect();
    let option = |l: &str| -> Option<(u32, String, bool)> {
        let t = l.trim_start();
        let (cursor, t) = match t.strip_prefix('❯') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, t),
        };
        let (num, rest) = t.split_once(". ")?;
        let n: u32 = num.parse().ok()?;
        let label = rest.trim().trim_start_matches("[ ] ").trim_start_matches("[✔] ").trim();
        (!label.is_empty()).then(|| (n, label.to_string(), cursor))
    };
    let Some(c) = lines.iter().rposition(|l| option(l).is_some_and(|(_, _, cur)| cur)) else { return vec![] };
    let (n0, label0, _) = option(lines[c]).expect("matched");
    let mut found = vec![(n0, label0)];
    // Up to the first option, then down to the last, each within a few lines (descriptions sit between).
    let mut i = c;
    let mut want = n0;
    while want > 1 {
        let hit = (i.saturating_sub(4)..i).rev().find(|j| option(lines[*j]).is_some_and(|(n, _, _)| n == want - 1));
        let Some(j) = hit else { return vec![] };
        let (n, label, _) = option(lines[j]).expect("matched");
        found.insert(0, (n, label));
        want -= 1;
        i = j;
    }
    let (mut i, mut want) = (c, n0 + 1);
    while let Some(j) = (i + 1..(i + 5).min(lines.len())).find(|j| option(lines[*j]).is_some_and(|(n, _, _)| n == want))
    {
        let (n, label, _) = option(lines[j]).expect("matched");
        found.push((n, label));
        want += 1;
        i = j;
    }
    found.into_iter().map(|(n, label)| PromptOption { n, label, description: None }).collect()
}

/// Fill in the options of permission and plan prompts from the screen, since the hook doesn't say them.
pub async fn fill_menus(node: std::sync::Arc<Node>) {
    let mut changes = node.registry.subscribe();
    let mut done: std::collections::HashSet<String> = Default::default();
    loop {
        let session = match changes.recv().await {
            Ok(Change::Updated { session }) => *session,
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let Some(p) = session.meta.blocked_prompt() else { continue };
        if p.kind == PromptKind::Question || !p.options.is_empty() || !done.insert(p.id.clone()) {
            continue;
        }
        if done.len() > 1000 {
            done.clear();
        }
        let (node, prompt_id) = (node.clone(), p.id.clone());
        tokio::spawn(async move {
            let Some(manager) = session.manager.as_deref().and_then(|m| node.plugin(m)) else { return };
            let Some(peer) = manager.peer().await else { return };
            // The menu draws a moment after the hook fires.
            for wait in [300, 1000, 2500] {
                tokio::time::sleep(Duration::from_millis(wait)).await;
                let Ok(text) = screen(&peer, &session.session_id).await else { return };
                let menu = parse_menu(&text);
                if !menu.is_empty() {
                    node.registry.set_prompt_options(&session.session_id, &prompt_id, menu, "screen").await;
                    return;
                }
            }
            tracing::debug!(session = %session.session_id, "no menu on screen for prompt {prompt_id}");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERMISSION: &str = " Bash command\n touch /tmp/x\n Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and always allow access to /tmp from this project\n   3. Yes, and switch to auto mode · auto mode handles these prompts for you\n   4. No\n Esc to cancel · Tab to amend\n";
    const PLAN: &str = "   1. Create the file.\n   2. Verify it.\n  ───\n   Would you like to proceed?\n   ❯ 1. Yes, and use auto mode\n     2. Yes, manually approve edits\n     3. Tell Claude what to change\n        shift+tab to approve with this feedback\n   ctrl+g to edit in Vim ·\n";

    fn labels(m: &[PromptOption]) -> Vec<&str> {
        m.iter().map(|o| o.label.as_str()).collect()
    }

    #[test]
    fn menus_are_read_around_the_cursor() {
        let m = parse_menu(PERMISSION);
        assert_eq!(m.len(), 4);
        assert_eq!(m[3], PromptOption { n: 4, label: "No".into(), description: None });
        // The plan's own numbered lines above aren't the menu.
        assert_eq!(
            labels(&parse_menu(PLAN)),
            ["Yes, and use auto mode", "Yes, manually approve edits", "Tell Claude what to change"]
        );
        let ask = "Which colour?\n❯ 1. Red\n     Pick red\n  2. Blue\n     Pick blue\n  3. Type something.\n───\n  4. Chat about this\n";
        assert_eq!(labels(&parse_menu(ask)), ["Red", "Blue", "Type something.", "Chat about this"]);
        let multi = "❯ 1. [✔] Small\n         Small size\n  2. [ ] Medium\n";
        assert_eq!(labels(&parse_menu(multi)), ["Small", "Medium"]);
        assert!(parse_menu("❯ \n idle prompt\n").is_empty());
    }

    fn question(multi: bool) -> fleetlayer_protocol::ext::PromptQuestion {
        fleetlayer_protocol::ext::PromptQuestion {
            header: "H".into(),
            question: "Which?".into(),
            multi_select: multi,
            options: (1..=3).map(|n| PromptOption { n, label: format!("O{n}"), description: None }).collect(),
            other: Some(4),
        }
    }

    fn prompt(kind: PromptKind, questions: Vec<fleetlayer_protocol::ext::PromptQuestion>) -> BlockedPrompt {
        BlockedPrompt {
            id: "toolu_1".into(),
            kind,
            at: jiff::Timestamp::now(),
            source: "hook".into(),
            questions,
            tool: None,
            plan: None,
            options: vec![],
            options_from: None,
        }
    }

    fn pick(n: u32) -> PromptAnswer {
        PromptAnswer { pick: Some(n), ..Default::default() }
    }

    fn keys(steps: &[KeyStep]) -> Vec<String> {
        steps
            .iter()
            .map(|s| match s {
                KeyStep::Key(k) => k.clone(),
                KeyStep::Text(t) => format!("text:{t}"),
            })
            .collect()
    }

    #[test]
    fn answers_become_the_keys_the_menus_take() {
        let form = prompt(PromptKind::Question, vec![question(false), question(true)]);
        let multi = PromptAnswer { picks: vec![1, 3], ..Default::default() };
        assert_eq!(keys(&keys_for(&form, &[], &[pick(2), multi.clone()]).unwrap()), ["2", "1", "3", "tab"]);
        assert!(needs_review(&form));
        let text = PromptAnswer { text: Some("A mango".into()), ..Default::default() };
        let single = prompt(PromptKind::Question, vec![question(false)]);
        assert_eq!(keys(&keys_for(&single, &[], std::slice::from_ref(&text)).unwrap()), ["4", "text:A mango", "enter"]);
        assert!(!needs_review(&single));
        // Wrong counts and out-of-range picks are refused before anything is typed.
        assert!(keys_for(&form, &[], &[pick(2)]).is_err());
        assert!(keys_for(&single, &[], &[pick(4)]).is_err());
        assert!(keys_for(&form, &[], &[pick(1), text.clone()]).is_err(), "no text in a multiple choice");

        let menu = parse_menu(PERMISSION);
        let perm = prompt(PromptKind::Permission, vec![]);
        assert_eq!(keys(&keys_for(&perm, &menu, &[pick(1)]).unwrap()), ["1"]);
        assert!(keys_for(&perm, &menu, &[pick(5)]).is_err());
        assert!(keys_for(&perm, &menu, std::slice::from_ref(&text)).is_err());
        let plan = prompt(PromptKind::Plan, vec![]);
        let plan_menu = parse_menu(PLAN);
        let feedback = PromptAnswer { text: Some("shorter".into()), ..Default::default() };
        assert_eq!(keys(&keys_for(&plan, &plan_menu, &[feedback]).unwrap()), ["3", "text:shorter", "enter"]);
    }

    #[test]
    fn a_prompt_counts_as_showing_only_when_the_screen_says_so() {
        let single = prompt(PromptKind::Question, vec![question(false)]);
        assert!(showing(&single, "Which?\n❯ 1. O1\n  2. O2", &[]));
        assert!(!showing(&single, "❯ \n idle", &[]));
        let mut perm = prompt(PromptKind::Permission, vec![]);
        let menu = parse_menu(PERMISSION);
        assert!(showing(&perm, PERMISSION, &menu));
        perm.options = menu.clone();
        assert!(showing(&perm, PERMISSION, &menu));
        // Another permission prompt with other options is not this one.
        assert!(!showing(&perm, PLAN, &parse_menu(PLAN)));
        assert!(!showing(&perm, "❯ ", &[]));
    }
}
