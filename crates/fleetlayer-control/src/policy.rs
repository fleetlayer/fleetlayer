//! Access control version 2 (`docs/access-control.md`): a policy file of groups and deny / only / allow rules, compiled
//! to Cedar policies and decided by Cedar's authorizer. Deny always wins (Cedar: a `forbid` beats any `permit`), so the
//! order of rules doesn't matter.
//!
//! Subjects (`who`) are `<tab>@<node>`, `-@<node>` or `plugin:<name>@<node>`; targets (`to`) are those, or
//! `tool:<name>@<node>`, `sync:<bundle>@<node>`, `channel:<name>`. `*` matches any run of characters; `group:<name>`
//! names a group of patterns.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use cedar_policy::{
    Authorizer, Context, Decision, Entities, Entity, EntityId, EntityTypeName, EntityUid, Policy, PolicyId, PolicySet,
    Request, RestrictedExpression,
};
use serde::{Deserialize, Serialize};

/// What a subject may do.
pub const VERBS: &[&str] = &["message", "use_tool", "sync", "read", "subscribe", "publish", "start"];

/// One string or a list of them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub fn items(&self) -> Vec<String> {
        match self {
            OneOrMany::One(s) => vec![s.clone()],
            OneOrMany::Many(v) => v.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub who: OneOrMany,
    pub can: OneOrMany,
    /// Targets; absent means every target.
    #[serde(default)]
    pub to: Option<OneOrMany>,
    /// Subjects the rule doesn't apply to, even though `who` matches them (a runner beside a restricted agent).
    #[serde(default)]
    pub except: Option<OneOrMany>,
    /// A note saying why, shown by `acl explain`.
    #[serde(default)]
    pub why: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limit {
    pub who: OneOrMany,
    #[serde(default)]
    pub messages_per_hour: Option<u32>,
    #[serde(default)]
    pub distinct_recipients_per_hour: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRules {
    #[serde(default)]
    pub deny: Vec<Rule>,
    #[serde(default)]
    pub only: Vec<Rule>,
}

/// The policy file, version 2.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    pub version: u32,
    pub default: String,
    #[serde(default)]
    pub groups: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub deny: Vec<Rule>,
    #[serde(default)]
    pub only: Vec<Rule>,
    #[serde(default)]
    pub allow: Vec<Rule>,
    /// Subjects whose messages are delivered as outside data.
    #[serde(default)]
    pub untrusted: Vec<String>,
    #[serde(default)]
    pub limits: Vec<Limit>,
    /// Rules that record what they would refuse, without refusing it.
    #[serde(default)]
    pub audit: AuditRules,
}

/// A decision, with the rules that made it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decided {
    pub allowed: bool,
    /// The rules behind it (`deny[0]`, `only[1]`, `allow[2]`, `default`), with their notes.
    pub because: Vec<String>,
    /// Audit rules that would have refused it.
    pub audit_would_deny: Vec<String>,
}

/// A compiled policy, ready to decide.
pub struct Compiled {
    pub file: PolicyFile,
    enforced: PolicySet,
    audit: PolicySet,
    notes: HashMap<String, String>,
}

fn quote(s: &str) -> String {
    // A Cedar string literal: escape backslashes and quotes. In `like` patterns `*` is the wildcard (patterns here use
    // `*` the same way), and a literal `*` can't be written, which these names never need.
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The Cedar condition for one `who` or `to` term against `var` (`principal` or `resource`).
fn term(var: &str, t: &str, groups: &BTreeMap<String, Vec<String>>) -> anyhow::Result<String> {
    if let Some(g) = t.strip_prefix("group:") {
        anyhow::ensure!(groups.contains_key(g), "unknown group {g}");
        return Ok(format!("{var} in Group::{}", quote(g)));
    }
    if t == "*" {
        return Ok("true".into());
    }
    anyhow::ensure!(valid_pattern(t), "{t:?} isn't <tab>@<node>, tool:/sync:/channel:…, group:… or *");
    Ok(format!("{var}.name like {}", quote(t)))
}

pub fn valid_pattern(p: &str) -> bool {
    p == "*" || p.contains('@') || p.starts_with("channel:") || p.starts_with("group:")
}

fn any(var: &str, terms: &[String], groups: &BTreeMap<String, Vec<String>>) -> anyhow::Result<String> {
    if terms.is_empty() {
        return Ok("false".into());
    }
    let parts: Vec<String> = terms.iter().map(|t| term(var, t, groups)).collect::<anyhow::Result<_>>()?;
    Ok(format!("({})", parts.join(" || ")))
}

/// The condition on the subject: matches `who` and none of `except`.
fn subject(r: &Rule, groups: &BTreeMap<String, Vec<String>>) -> anyhow::Result<String> {
    let who = any("principal", &r.who.items(), groups)?;
    Ok(match &r.except {
        Some(e) => format!("({who} && !{})", any("principal", &e.items(), groups)?),
        None => who,
    })
}

fn actions(can: &OneOrMany) -> anyhow::Result<String> {
    let verbs = can.items();
    for v in &verbs {
        anyhow::ensure!(VERBS.contains(&v.as_str()), "unknown verb {v} (verbs: {})", VERBS.join(", "));
    }
    let list: Vec<String> = verbs.iter().map(|v| format!("Action::{}", quote(v))).collect();
    Ok(format!("[{}]", list.join(", ")))
}

fn add(
    set: &mut PolicySet,
    notes: &mut HashMap<String, String>,
    id: String,
    note: Option<&str>,
    src: String,
) -> anyhow::Result<()> {
    let p = Policy::parse(Some(PolicyId::new(&id)), &src).map_err(|e| anyhow::anyhow!("{id}: {e}\n{src}"))?;
    set.add(p).map_err(|e| anyhow::anyhow!("{id}: {e}"))?;
    if let Some(n) = note {
        notes.insert(id, n.to_string());
    }
    Ok(())
}

/// `deny` and `only` rules as `forbid` policies, into `set`, with ids prefixed by `prefix`.
fn forbids(
    set: &mut PolicySet,
    notes: &mut HashMap<String, String>,
    prefix: &str,
    deny: &[Rule],
    only: &[Rule],
    groups: &BTreeMap<String, Vec<String>>,
) -> anyhow::Result<()> {
    for (i, r) in deny.iter().enumerate() {
        let who = subject(r, groups)?;
        let to = match &r.to {
            Some(t) => any("resource", &t.items(), groups)?,
            None => "true".into(),
        };
        let src = format!("forbid(principal, action in {}, resource) when {{ {who} && {to} }};", actions(&r.can)?);
        add(set, notes, format!("{prefix}deny[{i}]"), r.why.as_deref(), src)?;
    }
    for (i, r) in only.iter().enumerate() {
        let who = subject(r, groups)?;
        let to = match &r.to {
            Some(t) => any("resource", &t.items(), groups)?,
            None => "true".into(),
        };
        // Outside the list, and not a reply to a session that wrote first.
        let src = format!(
            "forbid(principal, action in {}, resource) when {{ {who} && !{to} && !context.reply }};",
            actions(&r.can)?
        );
        add(set, notes, format!("{prefix}only[{i}]"), r.why.as_deref(), src)?;
    }
    Ok(())
}

impl Compiled {
    pub fn compile(file: PolicyFile) -> anyhow::Result<Compiled> {
        anyhow::ensure!(file.version == 2, "this is a version {} policy; version 2 is expected", file.version);
        let default_allow = match file.default.as_str() {
            "allow" => true,
            "deny" => false,
            d => anyhow::bail!("default is allow or deny (got {d})"),
        };
        for (name, members) in &file.groups {
            for m in members {
                anyhow::ensure!(!m.starts_with("group:"), "group {name}: groups can't contain groups");
                anyhow::ensure!(valid_pattern(m), "group {name}: {m:?} isn't a pattern");
            }
        }
        let g = &file.groups;
        let mut enforced = PolicySet::new();
        let mut notes = HashMap::new();
        forbids(&mut enforced, &mut notes, "", &file.deny, &file.only, g)?;
        for (i, r) in file.allow.iter().enumerate() {
            let who = subject(r, g)?;
            let to = match &r.to {
                Some(t) => any("resource", &t.items(), g)?,
                None => "true".into(),
            };
            let src = format!("permit(principal, action in {}, resource) when {{ {who} && {to} }};", actions(&r.can)?);
            add(&mut enforced, &mut notes, format!("allow[{i}]"), r.why.as_deref(), src)?;
        }
        if default_allow {
            add(
                &mut enforced,
                &mut notes,
                "default".into(),
                Some("the default is allow"),
                "permit(principal, action, resource);".into(),
            )?;
        }
        for t in &file.untrusted {
            term("principal", t, g)?;
        }
        for l in &file.limits {
            any("principal", &l.who.items(), g)?;
        }
        // Audit rules: decided against everything allowed, so a forbid there means "would have refused".
        let mut audit = PolicySet::new();
        forbids(&mut audit, &mut notes, "audit.", &file.audit.deny, &file.audit.only, g)?;
        add(&mut audit, &mut notes, "audit.base".into(), None, "permit(principal, action, resource);".into())?;
        Ok(Compiled { file, enforced, audit, notes })
    }

    pub fn parse(text: &str) -> anyhow::Result<Compiled> {
        let file: PolicyFile = yaml_serde::from_str(text)?;
        Compiled::compile(file)
    }

    /// The groups a name belongs to.
    fn groups_of(&self, name: &str) -> HashSet<EntityUid> {
        self.file
            .groups
            .iter()
            .filter(|(_, members)| members.iter().any(|m| crate::acl::glob(m, name)))
            .map(|(g, _)| uid("Group", g))
            .collect()
    }

    /// Whether `who` belongs to any of `terms` (patterns or groups).
    pub fn matches(&self, terms: &[String], who: &str) -> bool {
        terms.iter().any(|t| match t.strip_prefix("group:") {
            Some(g) => self.file.groups.get(g).is_some_and(|ms| ms.iter().any(|m| crate::acl::glob(m, who))),
            None => crate::acl::glob(t, who),
        })
    }

    /// Whether messages from `who` are delivered as outside data.
    pub fn untrusted(&self, who: &str) -> bool {
        self.matches(&self.file.untrusted, who)
    }

    /// The tightest message limits that apply to `who`.
    pub fn limits_for(&self, who: &str) -> (Option<u32>, Option<u32>) {
        let mut per_hour: Option<u32> = None;
        let mut distinct: Option<u32> = None;
        for l in self.file.limits.iter().filter(|l| self.matches(&l.who.items(), who)) {
            if let Some(n) = l.messages_per_hour {
                per_hour = Some(per_hour.map_or(n, |p| p.min(n)));
            }
            if let Some(n) = l.distinct_recipients_per_hour {
                distinct = Some(distinct.map_or(n, |p| p.min(n)));
            }
        }
        (per_hour, distinct)
    }

    /// Decide whether `who` may `verb` `target`. `reply` is true when `target` messaged `who` within the reply window.
    pub fn decide(&self, who: &str, verb: &str, target: &str, reply: bool) -> anyhow::Result<Decided> {
        anyhow::ensure!(VERBS.contains(&verb), "unknown verb {verb}");
        let principal = uid("Agent", who);
        let resource = uid("Target", target);
        let mut name = HashMap::new();
        name.insert("name".to_string(), RestrictedExpression::new_string(who.to_string()));
        let p = Entity::new(principal.clone(), name, self.groups_of(who))?;
        let mut tname = HashMap::new();
        tname.insert("name".to_string(), RestrictedExpression::new_string(target.to_string()));
        let r = Entity::new(resource.clone(), tname, self.groups_of(target))?;
        let groups: Vec<Entity> = self
            .file
            .groups
            .keys()
            .map(|g| Entity::new(uid("Group", g), HashMap::new(), HashSet::new()).map_err(|e| anyhow::anyhow!("{e}")))
            .collect::<anyhow::Result<_>>()?;
        let entities = Entities::from_entities([p, r].into_iter().chain(groups), None)?;
        let context = Context::from_pairs([("reply".to_string(), RestrictedExpression::new_bool(reply))])?;
        let request = Request::new(principal, uid("Action", verb), resource, context, None)?;
        let authorizer = Authorizer::new();
        let resp = authorizer.is_authorized(&request, &self.enforced, &entities);
        if let Some(e) = resp.diagnostics().errors().next() {
            anyhow::bail!("deciding: {e}");
        }
        let allowed = resp.decision() == Decision::Allow;
        let mut because: Vec<String> = resp.diagnostics().reason().map(|id| self.describe(id)).collect();
        because.sort();
        if because.is_empty() && !allowed {
            because.push("no rule allows it (the default is deny)".into());
        }
        let mut audit_would_deny = vec![];
        if allowed {
            let a = authorizer.is_authorized(&request, &self.audit, &entities);
            if a.decision() == Decision::Deny {
                audit_would_deny = a.diagnostics().reason().map(|id| self.describe(id)).collect();
                audit_would_deny.sort();
            }
        }
        Ok(Decided { allowed, because, audit_would_deny })
    }

    fn describe(&self, id: &PolicyId) -> String {
        let id = id.to_string();
        match self.notes.get(&id) {
            Some(n) => format!("{id} ({n})"),
            None => id,
        }
    }
}

fn uid(kind: &str, id: &str) -> EntityUid {
    EntityUid::from_type_name_and_id(
        EntityTypeName::from_str(kind).expect("a fixed type name"),
        EntityId::from_str(id).expect("any string is an entity id"),
    )
}

// ---------------------------------------------------------------- suggestions

/// The smallest `only` rules that allow what each subject did: `observed` is (subject, verb, target). Subjects are
/// grouped by node (`*@node`) when `by_node`, so a rule covers the node's sessions together.
pub fn suggest(observed: &[(String, String, String)], by_node: bool) -> String {
    let mut by: BTreeMap<(String, String), std::collections::BTreeSet<String>> = BTreeMap::new();
    for (who, verb, target) in observed {
        let who = if by_node {
            who.rsplit_once('@').map_or(who.clone(), |(_, node)| format!("*@{node}"))
        } else {
            who.clone()
        };
        by.entry((who, verb.clone())).or_default().insert(target.clone());
    }
    if by.is_empty() {
        return "# Nothing observed in that time.\n".into();
    }
    let mut out = String::from(
        "# Suggested from what each subject did; review before adding them under only: (or audit: only:).\nonly:\n",
    );
    for ((who, verb), targets) in by {
        let list: Vec<String> = targets.iter().map(|t| format!("{t:?}")).collect();
        out.push_str(&format!("  - who: {who:?}\n    can: {verb}\n    to: [{}]\n", list.join(", ")));
    }
    out
}

// ---------------------------------------------------------------- tests of a policy

/// One expected decision, for `acl test`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub from: String,
    pub can: String,
    pub to: String,
    pub expect: String,
    #[serde(default)]
    pub reply: bool,
}

/// Check `cases` against `policy`; returns the failures as sentences.
pub fn run_cases(policy: &Compiled, cases: &[Case]) -> anyhow::Result<Vec<String>> {
    let mut failures = vec![];
    for c in cases {
        let want = match c.expect.as_str() {
            "allow" => true,
            "deny" => false,
            e => anyhow::bail!("expect is allow or deny (got {e})"),
        };
        let d = policy.decide(&c.from, &c.can, &c.to, c.reply)?;
        if d.allowed != want {
            failures.push(format!(
                "{} {} {}: expected {}, got {} because {}",
                c.from,
                c.can,
                c.to,
                c.expect,
                if d.allowed { "allow" } else { "deny" },
                d.because.join(", ")
            ));
        }
    }
    Ok(failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = r#"
version: 2
default: allow
groups:
  untrusted-input: ["*@triage", "*@scraper"]
  people: ["*@laptop", "*@server"]
deny:
  - who: group:untrusted-input
    can: [use_tool, sync]
    why: agents that read untrusted text get no tools
only:
  - who: "*@scraper"
    can: message
    to: ["research@laptop", "homeserver@server"]
allow:
  - who: group:people
    can: [message]
    to: "*"
untrusted: [group:untrusted-input]
limits:
  - who: group:untrusted-input
    messages_per_hour: 60
  - who: "*@scraper"
    messages_per_hour: 20
    distinct_recipients_per_hour: 3
audit:
  only:
    - who: "*@server"
      can: message
      to: ["*@server"]
"#;

    fn policy() -> Compiled {
        Compiled::parse(POLICY).unwrap()
    }

    #[test]
    fn deny_wins_whatever_the_order() {
        let p = policy();
        let d = p.decide("inbox@triage", "use_tool", "tool:browser@laptop", false).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.because, ["deny[0] (agents that read untrusted text get no tools)"]);
        // Allowed by the default, the broad allow for people doesn't matter for tools.
        assert!(p.decide("billing@laptop", "use_tool", "tool:browser@laptop", false).unwrap().allowed);
    }

    #[test]
    fn only_limits_targets_but_replies_pass() {
        let p = policy();
        assert!(p.decide("x@scraper", "message", "research@laptop", false).unwrap().allowed);
        let d = p.decide("x@scraper", "message", "billing@laptop", false).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.because, ["only[0]"]);
        assert!(p.decide("x@scraper", "message", "billing@laptop", true).unwrap().allowed, "a reply passes");
        assert!(p.decide("x@scraper", "read", "billing@laptop", false).unwrap().allowed, "only limits its verbs");
    }

    #[test]
    fn a_default_deny_needs_an_allow() {
        let p = Compiled::parse(
            "version: 2\ndefault: deny\nallow:\n  - { who: \"*@laptop\", can: message, to: \"*@laptop\" }\n",
        )
        .unwrap();
        assert!(p.decide("a@laptop", "message", "b@laptop", false).unwrap().allowed);
        let d = p.decide("a@laptop", "message", "b@server", false).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.because, ["no rule allows it (the default is deny)"]);
    }

    #[test]
    fn audit_rules_record_without_refusing() {
        let p = policy();
        let d = p.decide("ci@server", "message", "planning@laptop", false).unwrap();
        assert!(d.allowed);
        assert_eq!(d.audit_would_deny, ["audit.only[0]"]);
        assert!(p.decide("ci@server", "message", "logs@server", false).unwrap().audit_would_deny.is_empty());
    }

    #[test]
    fn untrusted_marking_and_the_tightest_limits() {
        let p = policy();
        assert!(p.untrusted("x@scraper"));
        assert!(!p.untrusted("x@laptop"));
        assert_eq!(p.limits_for("x@scraper"), (Some(20), Some(3)));
        assert_eq!(p.limits_for("x@triage"), (Some(60), None));
        assert_eq!(p.limits_for("x@laptop"), (None, None));
    }

    #[test]
    fn bad_policies_are_refused_with_a_reason() {
        let bad =
            |extra: &str| Compiled::parse(&format!("version: 2\ndefault: allow\n{extra}")).err().unwrap().to_string();
        assert!(bad("deny:\n  - { who: \"group:nope\", can: message }\n").contains("unknown group nope"));
        assert!(bad("deny:\n  - { who: \"*@x\", can: fly }\n").contains("unknown verb fly"));
        assert!(bad("deny:\n  - { who: \"nonsense\", can: message }\n").contains("isn't"));
        assert!(Compiled::parse("version: 1\ndefault: allow\n").is_err());
        assert!(bad("deni: []\n").contains("unknown field"));
    }

    #[test]
    fn names_with_quotes_cant_break_out_of_a_policy() {
        let p = Compiled::parse(
            "version: 2\ndefault: allow\ndeny:\n  - { who: \"a\\\" || true || \\\"@x\", can: message }\n",
        )
        .unwrap();
        assert!(p.decide("b@y", "message", "c@z", false).unwrap().allowed, "the quote stays inside the string");
    }

    #[test]
    fn except_exempts_a_runner_from_a_deny() {
        let p = Compiled::parse(
            "version: 2\ndefault: allow\ndeny:\n  - { who: \"*@triage\", except: \"plugin:runner@triage\", can: start }\n",
        )
        .unwrap();
        assert!(!p.decide("inbox@triage", "start", "x@laptop", false).unwrap().allowed);
        assert!(p.decide("plugin:runner@triage", "start", "x@laptop", false).unwrap().allowed);
    }

    #[test]
    fn suggestions_list_what_each_subject_did() {
        let seen = vec![
            ("inbox@triage".to_string(), "message".to_string(), "billing@laptop".to_string()),
            ("inbox@triage".to_string(), "message".to_string(), "ci@laptop".to_string()),
            ("other@triage".to_string(), "message".to_string(), "ci@laptop".to_string()),
            ("ci@laptop".to_string(), "use_tool".to_string(), "tool:browser@laptop".to_string()),
        ];
        let by_node = suggest(&seen, true);
        assert!(
            by_node.contains("who: \"*@triage\"\n    can: message\n    to: [\"billing@laptop\", \"ci@laptop\"]"),
            "{by_node}"
        );
        assert!(by_node.contains("can: use_tool\n    to: [\"tool:browser@laptop\"]"));
        // The suggestion parses as rules.
        let p = Compiled::parse(&format!("version: 2\ndefault: allow\n{by_node}")).unwrap();
        assert!(!p.decide("x@triage", "message", "finances@laptop", false).unwrap().allowed);
        assert!(suggest(&[], true).starts_with("# Nothing"));
    }

    #[test]
    fn cases_check_a_policy() {
        let cases: Vec<Case> = yaml_serde::from_str(
            "- { from: x@scraper, can: message, to: billing@laptop, expect: deny }\n- { from: x@scraper, can: message, to: research@laptop, expect: deny }\n",
        )
        .unwrap();
        let failures = run_cases(&policy(), &cases).unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].starts_with("x@scraper message research@laptop: expected deny, got allow"));
    }
}
