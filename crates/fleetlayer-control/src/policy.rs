//! Access control version 2 (`docs/access-control.md`): a policy file of groups and deny / only / allow rules, compiled
//! to Cedar policies and decided by Cedar's authorizer. Deny always wins (Cedar: a `forbid` beats any `permit`), so the
//! order of rules doesn't matter.
//!
//! Subjects (`who`) are `<tab>@<node>`, `-@<node>`, `plugin:<name>@<node>` or `person:<handle>`; targets (`to`) are
//! those, or `tool:<name>@<node>`, `sync:<bundle>@<node>`, `channel:<name>`, `service:<name>[/<owner>/…]`,
//! `page:<space>/<path>`, `app:<space>/<name>`. `*` matches any run of characters; `group:<name>` names a group of
//! patterns, `team:<name>` a team of the tenant (with its sub-teams), `role:admin` its admins, `owned-by:person:<h>` and
//! `owned-by:team:<t>` what someone owns, `owner` (in `to`) whatever the subject's owner owns, and `outside` an agent
//! that isn't a member of the tenant deciding. A pattern matches people only if it says so (`person:…`, `team:…`,
//! `role:…`): a bare `*` in `who` is every agent. People, teams and owners: `docs/sso.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use cedar_policy::{
    Authorizer, Context, Decision, Entities, Entity, EntityId, EntityTypeName, EntityUid, Policy, PolicyId, PolicySet,
    Request, RestrictedExpression,
};
use serde::{Deserialize, Serialize};

use crate::directory::Fleet;

/// What a subject may do.
pub const VERBS: &[&str] =
    &["message", "use_tool", "sync", "read", "subscribe", "publish", "start", "attach", "sign_in", "admin", "write"];

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

/// An `ask` rule: allowed once the person approves; an approval holds for `for`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AskRule {
    pub who: OneOrMany,
    pub can: OneOrMany,
    #[serde(default)]
    pub to: Option<OneOrMany>,
    #[serde(default)]
    pub except: Option<OneOrMany>,
    /// How long an approval holds: `10m`, `2h`, `30d`.
    #[serde(rename = "for")]
    pub hold: String,
}

/// `10m`, `2h`, `30d` in milliseconds.
pub fn parse_duration_ms(s: &str) -> anyhow::Result<i64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = n.parse().map_err(|_| anyhow::anyhow!("{s:?} isn't like 10m, 2h or 30d"))?;
    Ok(n * match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => anyhow::bail!("{s:?} isn't like 10m, 2h or 30d"),
    })
}

/// Fleetlayer's default `ask:` when a policy leaves it out: on by default, the safe start for a fleet you don't know
/// yet. A fleet of autonomous agents writes `ask: []`.
pub fn default_ask() -> Vec<AskRule> {
    let rule = |can: &[&str], hold: &str| AskRule {
        who: OneOrMany::One("*".into()),
        can: OneOrMany::Many(can.iter().map(|c| c.to_string()).collect()),
        to: None,
        except: None,
        hold: hold.into(),
    };
    vec![rule(&["start", "attach"], "10m"), rule(&["use_tool", "sync", "admin", "write"], "30d")]
}

/// What an approval is held for: one target, except pages, approved per space (`page:<space>/*`).
pub fn approval_target(target: &str) -> String {
    match target.strip_prefix("page:").and_then(|rest| rest.split_once('/')) {
        Some((space, _)) => format!("page:{space}/*"),
        None => target.to_string(),
    }
}

/// The policy file, version 2.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    pub version: u32,
    pub default: String,
    /// What people may do when no rule says: `own` (the default: anything to their own things, and read their teams'),
    /// `deny` or `allow`. `default:` keeps deciding for agents.
    #[serde(default)]
    pub default_people: Option<String>,
    /// `deny` refuses every action by or to this tenant's member agents that are members of another tenant too.
    #[serde(default)]
    pub agents_elsewhere: Option<String>,
    #[serde(default)]
    pub groups: BTreeMap<String, Vec<String>>,
    /// Who owns what, for things whose owner isn't known otherwise (spaces of pages, apps): a pattern and its owner
    /// (`person:<handle>` or `team:<name>`). The most specific (longest) matching pattern wins.
    #[serde(default)]
    pub owners: BTreeMap<String, String>,
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
    /// Allowed once the person approves. Absent: Fleetlayer's defaults ([`default_ask`]); `[]`: never ask.
    #[serde(default)]
    pub ask: Option<Vec<AskRule>>,
    /// Requests that must say why (one line, recorded in the audit log). None by default.
    #[serde(default)]
    pub justify: Vec<Rule>,
}

/// Check a justification: one line of plain text, 16 to 1000 characters, nothing hidden (after Rack Gateway's).
pub fn check_justification(text: &str) -> Result<(), String> {
    for c in text.chars() {
        if c == '\n' || c == '\r' || c == '\t' {
            return Err("a justification is one line of plain text".into());
        }
        if c.is_control()
            || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
        {
            return Err(format!("a justification can't hold hidden or control characters (it has {:?})", c));
        }
    }
    let n = text.trim().chars().count();
    if n < 16 {
        return Err(format!(
            "the justification is too short ({n} characters, at least 16): say why, and what it's for"
        ));
    }
    if n > 1000 {
        return Err(format!("the justification is too long ({n} characters, at most 1000)"));
    }
    Ok(())
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

/// The tenant a decision is made in, with the deployment's membership (`directory.rs`). Without one, nobody's
/// membership is checked and only `owners:` and `service:` paths say who owns what (`acl explain --policy`, tests).
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    pub fleet: &'a Fleet,
    pub tenant: &'a crate::directory::Tenant,
}

/// What an owner is written as: `person:<handle>` or `team:<name>`.
fn valid_owner(o: &str) -> bool {
    o.strip_prefix("person:").or_else(|| o.strip_prefix("team:")).is_some_and(valid_slug)
}

/// A handle, team or role name: a lowercase slug of at most 64 characters, with no `@` (so it can't clash with
/// `<tab>@<node>`). A pattern may hold `*`.
fn valid_slug_pattern(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '*'))
}

fn valid_slug(n: &str) -> bool {
    !n.contains('*') && valid_slug_pattern(n)
}

/// The Cedar condition for one `who` or `to` term against `var` (`principal` or `resource`).
fn term(var: &str, t: &str, groups: &BTreeMap<String, Vec<String>>) -> anyhow::Result<String> {
    let subject = var == "principal";
    if let Some(g) = t.strip_prefix("group:") {
        anyhow::ensure!(groups.contains_key(g), "unknown group {g}");
        return Ok(format!("{var} in Group::{}", quote(g)));
    }
    if let Some(o) = t.strip_prefix("owned-by:") {
        anyhow::ensure!(valid_owner(o), "{t:?} isn't owned-by:person:<handle> or owned-by:team:<name>");
        return Ok(format!("{var}.owner == {}", quote(o)));
    }
    if let Some(team) = t.strip_prefix("team:") {
        anyhow::ensure!(valid_slug(team), "{t:?}: {team:?} isn't a team's name");
        return Ok(format!("{var} in Team::{}", quote(team)));
    }
    if let Some(role) = t.strip_prefix("role:") {
        anyhow::ensure!(matches!(role, "admin" | "member"), "{t:?}: roles are admin and member");
        return Ok(format!("{var} in Role::{}", quote(role)));
    }
    if t == "owner" {
        anyhow::ensure!(!subject, "`owner` names targets: use it in `to`, not `who`");
        return Ok("context.same_owner".into());
    }
    if t == "outside" {
        return Ok(format!("{var}.outside"));
    }
    if t == "*" {
        // In `who`, a bare `*` means every agent, never people.
        return Ok(if subject { "!principal.person".into() } else { "true".into() });
    }
    anyhow::ensure!(
        valid_pattern(t),
        "{t:?} isn't <tab>@<node>, person:/team:/role:…, tool:/sync:/channel:/service:/page:/app:…, group:…, \
         owned-by:…, owner, outside or *"
    );
    if let Some(h) = t.strip_prefix("person:") {
        anyhow::ensure!(valid_slug_pattern(h), "{t:?}: {h:?} isn't a handle");
    }
    if subject && !t.starts_with("person:") {
        return Ok(format!("(!principal.person && principal.name like {})", quote(t)));
    }
    Ok(format!("{var}.name like {}", quote(t)))
}

pub fn valid_pattern(p: &str) -> bool {
    p == "*"
        || (p.contains('@') && !p.starts_with("person:"))
        || ["channel:", "group:", "person:", "service:", "page:", "app:"]
            .iter()
            .any(|k| p.starts_with(k) && p.len() > k.len())
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

/// Which side of a request a name is on, for terms that mean something different on each (`*`, `owner`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Who,
    To,
}

impl Compiled {
    pub fn compile(file: PolicyFile) -> anyhow::Result<Compiled> {
        anyhow::ensure!(file.version == 2, "this is a version {} policy; version 2 is expected", file.version);
        let default_allow = match file.default.as_str() {
            "allow" => true,
            "deny" => false,
            d => anyhow::bail!("default is allow or deny (got {d})"),
        };
        let people_default = file.default_people.as_deref().unwrap_or("own");
        anyhow::ensure!(
            matches!(people_default, "own" | "deny" | "allow"),
            "default_people is own, deny or allow (got {people_default})"
        );
        if let Some(e) = &file.agents_elsewhere {
            anyhow::ensure!(matches!(e.as_str(), "allow" | "deny"), "agents_elsewhere is allow or deny (got {e})");
        }
        for (name, members) in &file.groups {
            for m in members {
                anyhow::ensure!(!m.starts_with("group:"), "group {name}: groups can't contain groups");
                let ok = valid_pattern(m)
                    || m.strip_prefix("team:").is_some_and(valid_slug)
                    || matches!(m.as_str(), "role:admin" | "role:member");
                anyhow::ensure!(ok, "group {name}: {m:?} isn't a pattern, team:<name> or role:<role>");
            }
        }
        for (pattern, owner) in &file.owners {
            anyhow::ensure!(valid_pattern(pattern) && pattern != "*", "owners: {pattern:?} isn't a pattern");
            anyhow::ensure!(
                valid_owner(owner),
                "owners: {pattern}'s owner {owner:?} isn't person:<handle> or team:<name>"
            );
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
                Some("the default for agents is allow"),
                "permit(principal, action, resource) when { !principal.person };".into(),
            )?;
        }
        match people_default {
            "own" => add(
                &mut enforced,
                &mut notes,
                "default_people".into(),
                Some("people may do anything to what they own, and read what their teams own"),
                "permit(principal, action, resource) when { principal.person && (context.same_owner || \
                 (action == Action::\"read\" && context.team_owns)) };"
                    .into(),
            )?,
            "allow" => add(
                &mut enforced,
                &mut notes,
                "default_people".into(),
                Some("the default for people is allow"),
                "permit(principal, action, resource) when { principal.person };".into(),
            )?,
            _ => {}
        }
        for t in &file.untrusted {
            term("principal", t, g)?;
        }
        for r in &file.justify {
            any("principal", &r.who.items(), g)?;
            actions(&r.can)?;
        }
        for r in file.ask.iter().flatten() {
            parse_duration_ms(&r.hold)?;
            any("principal", &r.who.items(), g)?;
            actions(&r.can)?;
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

    /// The member `name` is, when it's a person in the scope's tenant.
    fn person<'a>(scope: Option<Scope<'a>>, name: &str) -> Option<(&'a str, &'a crate::directory::Person)> {
        let h = name.strip_prefix("person:")?;
        let (handle, p) = scope?.tenant.people.get_key_value(h)?;
        Some((handle.as_str(), p))
    }

    /// The teams a person is in (with the teams above them); empty for agents.
    fn teams(scope: Option<Scope>, name: &str) -> std::collections::BTreeSet<String> {
        match (scope, name.strip_prefix("person:")) {
            (Some(s), Some(h)) => s.tenant.teams_of(h),
            _ => Default::default(),
        }
    }

    /// Whether a group member (a pattern, `team:…` or `role:…`) takes in `name`.
    fn group_member(scope: Option<Scope>, m: &str, name: &str) -> bool {
        if let Some(team) = m.strip_prefix("team:") {
            return Compiled::teams(scope, name).contains(team);
        }
        if let Some(role) = m.strip_prefix("role:") {
            return Compiled::person(scope, name).is_some_and(|(_, p)| p.role == role);
        }
        subject_glob(m, name)
    }

    /// The names of the policy's groups `name` belongs to.
    fn group_names(&self, scope: Option<Scope>, name: &str) -> HashSet<String> {
        self.file
            .groups
            .iter()
            .filter(|(_, members)| members.iter().any(|m| Compiled::group_member(scope, m, name)))
            .map(|(g, _)| g.clone())
            .collect()
    }

    /// Who owns `name`, in the scope's tenant: `person:<handle>`, `team:<name>`, `tenant` (a tenant's own node), or ""
    /// when nobody does. `owners:` first (the most specific pattern), then a person themselves, then a service's path
    /// (`service:<name>/<owner>/…`), then a node's owner for what's on it.
    pub fn owner_of(&self, scope: Option<Scope>, name: &str) -> String {
        let mut candidates = vec![name.to_string()];
        if crate::directory::is_agent_side(name)
            && let Some((_, node)) = name.rsplit_once('@')
        {
            candidates.push(format!("-@{node}"));
        }
        for c in &candidates {
            if let Some((_, owner)) =
                self.file.owners.iter().filter(|(p, _)| crate::acl::glob(p, c)).max_by_key(|(p, _)| p.len())
            {
                return owner.clone();
            }
        }
        if name.starts_with("person:") {
            return name.to_string();
        }
        if let Some(rest) = name.strip_prefix("service:")
            && let Some((_, after)) = rest.split_once('/')
        {
            let seg = after.split('/').next().unwrap_or_default();
            return match seg.strip_prefix("team:") {
                Some(t) if valid_slug(t) => format!("team:{t}"),
                None if valid_slug(seg) => format!("person:{seg}"),
                _ => String::new(),
            };
        }
        scope.map(|s| s.tenant.node_owner(s.fleet, name)).unwrap_or_default()
    }

    /// Whether `name` isn't a member of the scope's tenant (`outside`). Never true without a scope.
    fn outside(scope: Option<Scope>, name: &str) -> bool {
        scope.is_some_and(|s| !name.starts_with("service:") && !s.tenant.is_member(s.fleet, name))
            && (crate::directory::is_agent_side(name) || name.starts_with("person:"))
    }

    /// Whether `name`, on `side`, matches one term; `other` is the request's other side (for `owner`).
    fn term_matches(&self, scope: Option<Scope>, t: &str, name: &str, side: Side, other: &str) -> bool {
        if let Some(g) = t.strip_prefix("group:") {
            return self.group_names(scope, name).contains(g);
        }
        if let Some(o) = t.strip_prefix("owned-by:") {
            return self.owner_of(scope, name) == o;
        }
        if t.starts_with("team:") || t.starts_with("role:") {
            return Compiled::group_member(scope, t, name);
        }
        match t {
            "outside" => Compiled::outside(scope, name),
            "owner" => {
                let mine = self.owner_of(scope, other);
                side == Side::To && !mine.is_empty() && mine == self.owner_of(scope, name)
            }
            "*" if side == Side::To => true,
            _ => subject_glob(t, name),
        }
    }

    fn any_matches(&self, scope: Option<Scope>, terms: &[String], name: &str, side: Side, other: &str) -> bool {
        terms.iter().any(|t| self.term_matches(scope, t, name, side, other))
    }

    /// Whether `who` doing `verb` to `target` is covered by a rule's who, except, can and to.
    fn covers(
        &self,
        scope: Option<Scope>,
        (who_terms, except, can, to): (&OneOrMany, &Option<OneOrMany>, &OneOrMany, &Option<OneOrMany>),
        (who, verb, target): (&str, &str, &str),
    ) -> bool {
        can.items().iter().any(|c| c == verb)
            && self.any_matches(scope, &who_terms.items(), who, Side::Who, target)
            && !except.as_ref().is_some_and(|e| self.any_matches(scope, &e.items(), who, Side::Who, target))
            && to.as_ref().is_none_or(|t| self.any_matches(scope, &t.items(), target, Side::To, who))
    }

    /// Whether `who` belongs to any of `terms` (patterns, groups, teams, roles).
    pub fn matches(&self, terms: &[String], who: &str) -> bool {
        self.any_matches(None, terms, who, Side::Who, "")
    }

    /// How long an approval holds when `who` doing `verb` to `target` needs one; None when it doesn't.
    pub fn ask_for(&self, who: &str, verb: &str, target: &str) -> Option<i64> {
        self.ask_for_in(None, who, verb, target)
    }

    pub fn ask_for_in(&self, scope: Option<Scope>, who: &str, verb: &str, target: &str) -> Option<i64> {
        let defaults;
        let rules = match &self.file.ask {
            Some(r) => r,
            None => {
                defaults = default_ask();
                &defaults
            }
        };
        rules
            .iter()
            .filter(|r| self.covers(scope, (&r.who, &r.except, &r.can, &r.to), (who, verb, target)))
            .filter_map(|r| parse_duration_ms(&r.hold).ok())
            .min()
    }

    /// Whether `who` doing `verb` to `target` must say why.
    pub fn needs_justification(&self, who: &str, verb: &str, target: &str) -> bool {
        self.needs_justification_in(None, who, verb, target)
    }

    pub fn needs_justification_in(&self, scope: Option<Scope>, who: &str, verb: &str, target: &str) -> bool {
        self.file.justify.iter().any(|r| self.covers(scope, (&r.who, &r.except, &r.can, &r.to), (who, verb, target)))
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

    /// Decide whether `who` may `verb` `target`, without a tenant's membership. `reply` is true when `target` messaged
    /// `who` within the reply window.
    pub fn decide(&self, who: &str, verb: &str, target: &str, reply: bool) -> anyhow::Result<Decided> {
        self.decide_in(None, who, verb, target, reply)
    }

    /// Why the scope's tenant refuses before reading any rule: a subject that isn't a member (except between agents,
    /// where it's `outside`), or a member agent that's also a member elsewhere under `agents_elsewhere: deny`.
    pub fn refused_up_front(&self, scope: Scope, who: &str, target: &str) -> Option<String> {
        let (t, f) = (scope.tenant, scope.fleet);
        let member = t.is_member(f, who);
        if !member && (who.starts_with("person:") || !crate::directory::is_agent_side(target)) {
            return Some(format!("{who} isn't a member of {}", t.slug));
        }
        if self.file.agents_elsewhere.as_deref() == Some("deny") {
            for n in [who, target] {
                if crate::directory::is_agent_side(n) && t.is_member(f, n) && f.elsewhere(&t.id, n) {
                    return Some(format!(
                        "{n} is also a member of another tenant, and {} refuses those (agents_elsewhere: deny)",
                        t.slug
                    ));
                }
            }
        }
        None
    }

    /// Decide in a tenant: membership first, then the policy's rules.
    pub fn decide_in(
        &self,
        scope: Option<Scope>,
        who: &str,
        verb: &str,
        target: &str,
        reply: bool,
    ) -> anyhow::Result<Decided> {
        anyhow::ensure!(VERBS.contains(&verb), "unknown verb {verb}");
        if let Some(s) = scope
            && let Some(r) = self.refused_up_front(s, who, target)
        {
            return Ok(Decided { allowed: false, because: vec![r], audit_would_deny: vec![] });
        }
        let person = who.starts_with("person:");
        let principal = uid("Agent", who);
        let resource = uid("Target", target);
        let (who_owner, target_owner) = (self.owner_of(scope, who), self.owner_of(scope, target));
        let attrs = |n: &str, owner: &str| {
            HashMap::from([
                ("name".to_string(), RestrictedExpression::new_string(n.to_string())),
                ("person".to_string(), RestrictedExpression::new_bool(n.starts_with("person:"))),
                ("owner".to_string(), RestrictedExpression::new_string(owner.to_string())),
                ("outside".to_string(), RestrictedExpression::new_bool(Compiled::outside(scope, n))),
            ])
        };
        let parents = |n: &str| -> HashSet<EntityUid> {
            let mut out: HashSet<EntityUid> = self.group_names(scope, n).iter().map(|g| uid("Group", g)).collect();
            out.extend(Compiled::teams(scope, n).iter().map(|t| uid("Team", t)));
            if let Some((_, p)) = Compiled::person(scope, n) {
                out.insert(uid("Role", &p.role));
            }
            out
        };
        let (who_parents, target_parents) = (parents(who), parents(target));
        let mut others: HashSet<EntityUid> = self.file.groups.keys().map(|g| uid("Group", g)).collect();
        others.extend(who_parents.iter().cloned());
        others.extend(target_parents.iter().cloned());
        let p = Entity::new(principal.clone(), attrs(who, &who_owner), who_parents)?;
        let r = Entity::new(resource.clone(), attrs(target, &target_owner), target_parents)?;
        let others: Vec<Entity> = others
            .into_iter()
            .map(|u| Entity::new(u, HashMap::new(), HashSet::new()).map_err(|e| anyhow::anyhow!("{e}")))
            .collect::<anyhow::Result<_>>()?;
        let entities = Entities::from_entities([p, r].into_iter().chain(others), None)?;
        let same_owner = !who_owner.is_empty() && who_owner == target_owner;
        let team_owns =
            target_owner.strip_prefix("team:").is_some_and(|t| person && Compiled::teams(scope, who).contains(t));
        let context = Context::from_pairs([
            ("reply".to_string(), RestrictedExpression::new_bool(reply)),
            ("same_owner".to_string(), RestrictedExpression::new_bool(same_owner)),
            ("team_owns".to_string(), RestrictedExpression::new_bool(team_owns)),
        ])?;
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
            because.push(if person {
                match self.file.default_people.as_deref().unwrap_or("own") {
                    "own" => "no rule allows it (people may do only what they own, and read their teams', unless a \
                              rule allows more)"
                        .into(),
                    _ => "no rule allows it (the default for people is deny)".into(),
                }
            } else {
                "no rule allows it (the default is deny)".into()
            });
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

/// Whether pattern `p` matches `name`: people only through a `person:` pattern, so `*` and `*@laptop` never do.
fn subject_glob(p: &str, name: &str) -> bool {
    if name.starts_with("person:") != p.starts_with("person:") {
        return false;
    }
    crate::acl::glob(p, name)
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
    /// For a person (`from: person:<handle>`): their role in the tenant (`admin` or `member`, the default) and teams.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub teams: Vec<String>,
}

impl Case {
    /// A tenant for a case about a person: that person, with the role and teams the case gives, and every agent
    /// admitted. None for a case about agents, decided without membership.
    fn tenant(&self) -> Option<crate::directory::Tenant> {
        let handle = self.from.strip_prefix("person:")?;
        let mut t = crate::directory::Tenant { id: "test".into(), slug: "test".into(), ..Default::default() };
        let person = crate::directory::Person {
            id: "p_test".into(),
            role: self.role.clone().unwrap_or_else(|| "member".into()),
            teams: self.teams.clone(),
        };
        t.people.insert(handle.into(), person);
        t.handles.insert("p_test".into(), handle.into());
        t.agents.push("*".into());
        for team in &self.teams {
            t.teams.insert(team.clone(), None);
        }
        Some(t)
    }
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
        let tenant = c.tenant();
        let fleet = Fleet::default();
        let scope = tenant.as_ref().map(|tenant| Scope { fleet: &fleet, tenant });
        let d = policy.decide_in(scope, &c.from, &c.can, &c.to, c.reply)?;
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
    fn approvals_are_on_by_default_and_off_with_an_empty_list() {
        let base = "version: 2\ndefault: allow\n";
        let p = Compiled::parse(base).unwrap();
        assert_eq!(p.ask_for("a@x", "start", "b@y"), Some(600_000));
        assert_eq!(p.ask_for("a@x", "use_tool", "tool:browser@y"), Some(30 * 86_400_000));
        assert_eq!(p.ask_for("a@x", "message", "b@y"), None);
        let off = Compiled::parse(&format!("{base}ask: []\n")).unwrap();
        assert_eq!(off.ask_for("a@x", "start", "b@y"), None);
        let some =
            Compiled::parse(&format!("{base}ask:\n  - {{ who: \"*@triage\", can: message, for: 2h }}\n")).unwrap();
        assert_eq!(some.ask_for("x@triage", "message", "b@y"), Some(7_200_000));
        assert_eq!(some.ask_for("x@laptop", "message", "b@y"), None);
        assert!(Compiled::parse(&format!("{base}ask:\n  - {{ who: \"*\", can: start, for: soon }}\n")).is_err());
    }

    #[test]
    fn justifications_are_required_by_rule_and_checked() {
        let p = Compiled::parse(
            "version: 2\ndefault: allow\njustify:\n  - { who: \"*\", can: use_tool, to: \"tool:browser@*\" }\n",
        )
        .unwrap();
        assert!(p.needs_justification("a@x", "use_tool", "tool:browser@laptop"));
        assert!(!p.needs_justification("a@x", "use_tool", "tool:db@laptop"));
        assert!(!p.needs_justification("a@x", "message", "b@y"));
        assert!(check_justification("Checking the invoice page renders for sc-1234").is_ok());
        assert!(check_justification("because").unwrap_err().contains("too short"));
        assert!(check_justification("two lines of text\nare not one line").is_err());
        assert!(check_justification("hidden\u{202e}override characters here").unwrap_err().contains("hidden"));
    }

    /// The company policy from `docs/sso.md`.
    const COMPANY: &str = r#"
version: 2
default: allow
groups:
  staff: [team:engineering, team:support]
allow:
  - who: role:admin
    can: [sign_in, admin]
    to: ["service:*"]
  - who: group:staff
    can: [sign_in]
    to: [service:pages]
  - who: group:staff
    can: [read]
    to: ["page:handbook/*"]
deny:
  - who: "person:*"
    except: [role:admin]
    can: [sign_in]
    to: [service:hub]
    why: The hub holds every Claude sign-in
owners:
  "page:handbook/*": team:support
  "page:notes-c/*": person:c
"#;

    fn acme<'a>(f: &'a Fleet) -> Scope<'a> {
        Scope { fleet: f, tenant: f.tenant("acme").unwrap() }
    }

    #[test]
    fn people_get_what_the_company_policy_says() {
        let f = crate::directory::tests::example();
        let p = Compiled::parse(COMPANY).unwrap();
        let s = Some(acme(&f));
        let ok = |who: &str, can: &str, to: &str| p.decide_in(s, who, can, to, false).unwrap().allowed;
        // C (admin, platform under engineering) and D (member, support).
        assert!(ok("person:c", "sign_in", "service:hub"));
        assert!(ok("person:c", "admin", "service:signin"));
        assert!(!ok("person:d", "sign_in", "service:hub"), "the hub is for admins");
        assert!(ok("person:d", "sign_in", "service:pages"));
        assert!(ok("person:c", "sign_in", "service:pages"), "platform is in engineering, which is staff");
        assert!(ok("person:d", "read", "page:handbook/start"));
        assert!(!ok("person:d", "admin", "service:signin"));
        // Their own things, and their teams', by default; nobody else's, whatever `default: allow` says for agents.
        assert!(ok("person:c", "write", "page:notes-c/today"));
        assert!(!ok("person:d", "read", "page:notes-c/today"));
        assert!(!ok("person:d", "write", "page:handbook/start"), "a team's members read its things by default");
    }

    #[test]
    fn membership_comes_before_any_rule() {
        let f = crate::directory::tests::example();
        let p = Compiled::parse("version: 2\ndefault: allow\ndefault_people: allow\n").unwrap();
        let s = Some(acme(&f));
        let d = p.decide_in(s, "person:b", "sign_in", "service:pages", false).unwrap();
        assert!(!d.allowed);
        assert_eq!(d.because, ["person:b isn't a member of acme"]);
        let d = p.decide_in(s, "recipes@laptop", "read", "service:quietdeck/a/inbox", false).unwrap();
        assert_eq!(d.because, ["recipes@laptop isn't a member of acme"]);
        // Between agents, a non-member is an outside agent, for the rules to decide.
        assert!(p.decide_in(s, "recipes@laptop", "message", "billing@laptop", false).unwrap().allowed);
        let own = Compiled::parse(
            "version: 2\ndefault: allow\ndeny:\n  - { who: outside, can: message }\n  - { who: \"*\", can: message, to: outside }\n",
        )
        .unwrap();
        assert!(!own.decide_in(s, "recipes@laptop", "message", "billing@laptop", false).unwrap().allowed);
        assert!(!own.decide_in(s, "billing@laptop", "message", "recipes@laptop", false).unwrap().allowed);
        assert!(own.decide_in(s, "billing@laptop", "message", "x@c-vm", false).unwrap().allowed);
    }

    #[test]
    fn agents_elsewhere_can_be_refused() {
        let f = crate::directory::tests::example();
        let p = Compiled::parse("version: 2\ndefault: allow\nagents_elsewhere: deny\n").unwrap();
        let s = Some(acme(&f));
        let d = p.decide_in(s, "finances@laptop", "read", "channel:ledger", false).unwrap();
        assert!(!d.allowed);
        assert!(d.because[0].contains("agents_elsewhere: deny"), "{:?}", d.because);
        assert!(!p.decide_in(s, "billing@laptop", "message", "finances@laptop", false).unwrap().allowed);
        assert!(p.decide_in(s, "billing@laptop", "message", "x@c-vm", false).unwrap().allowed);
    }

    #[test]
    fn a_bare_star_never_means_people() {
        let f = crate::directory::tests::example();
        let s = Some(acme(&f));
        let p = Compiled::parse(
            "version: 2\ndefault: deny\ndefault_people: deny\nallow:\n  - { who: \"*\", can: read, to: \"*\" }\n",
        )
        .unwrap();
        assert!(p.decide_in(s, "billing@laptop", "read", "page:x/y", false).unwrap().allowed);
        assert!(!p.decide_in(s, "person:a", "read", "page:x/y", false).unwrap().allowed);
        // An ask for `*` doesn't stop people either.
        assert_eq!(
            Compiled::parse("version: 2\ndefault: allow\n").unwrap().ask_for_in(
                s,
                "person:a",
                "admin",
                "service:signin"
            ),
            None
        );
    }

    #[test]
    fn owners_and_owner_scoped_rules() {
        let f = crate::directory::tests::example();
        let s = Some(acme(&f));
        let p = Compiled::parse(
            "version: 2\ndefault: deny\nallow:\n  - { who: \"*\", can: read, to: [owner] }\n  - { who: owned-by:person:c, can: message, to: owned-by:team:engineering }\n",
        )
        .unwrap();
        assert_eq!(p.owner_of(s, "billing@laptop"), "person:a");
        assert_eq!(p.owner_of(s, "service:quietdeck/a/inbox"), "person:a");
        assert_eq!(p.owner_of(s, "service:quietdeck/team:support/inbox"), "team:support");
        assert_eq!(p.owner_of(s, "tool:shell@build-vm"), "team:engineering");
        // An agent reads its person's things, and nobody else's.
        assert!(p.decide_in(s, "billing@laptop", "read", "service:quietdeck/a/inbox", false).unwrap().allowed);
        assert!(!p.decide_in(s, "billing@laptop", "read", "service:quietdeck/c/inbox", false).unwrap().allowed);
        assert!(p.decide_in(s, "x@c-vm", "message", "y@build-vm", false).unwrap().allowed);
        assert!(!p.decide_in(s, "billing@laptop", "message", "y@build-vm", false).unwrap().allowed);
    }

    #[test]
    fn people_cases_say_their_role_and_teams() {
        let cases: Vec<Case> = yaml_serde::from_str(
            "- { from: person:sam, can: sign_in, to: service:hub, expect: deny }\n- { from: person:sam, role: admin, can: sign_in, to: service:hub, expect: allow }\n- { from: person:sam, teams: [support], can: read, to: page:handbook/start, expect: allow }\n- { from: person:sam, can: read, to: page:handbook/start, expect: deny }\n",
        )
        .unwrap();
        let failures = run_cases(&Compiled::parse(COMPANY).unwrap(), &cases).unwrap();
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn new_verbs_ask_by_default_and_pages_are_approved_per_space() {
        let p = Compiled::parse("version: 2\ndefault: allow\n").unwrap();
        assert_eq!(p.ask_for("a@x", "write", "page:family/notes.md"), Some(30 * 86_400_000));
        assert_eq!(p.ask_for("a@x", "admin", "service:grafana"), Some(30 * 86_400_000));
        assert_eq!(p.ask_for("a@x", "sign_in", "service:grafana"), None);
        assert_eq!(p.ask_for("a@x", "read", "page:family/notes.md"), None);
        assert_eq!(approval_target("page:family/2026/notes.md"), "page:family/*");
        assert_eq!(approval_target("tool:browser@laptop"), "tool:browser@laptop");
    }

    #[test]
    fn bad_people_terms_are_refused() {
        let bad =
            |extra: &str| Compiled::parse(&format!("version: 2\ndefault: allow\n{extra}")).err().unwrap().to_string();
        assert!(bad("allow:\n  - { who: role:owner, can: read }\n").contains("roles are admin and member"));
        assert!(bad("allow:\n  - { who: owner, can: read }\n").contains("use it in `to`"));
        assert!(bad("allow:\n  - { who: \"person:A@x\", can: read }\n").contains("isn't"));
        assert!(bad("default_people: some\n").contains("default_people"));
        assert!(bad("owners:\n  \"page:x/*\": david\n").contains("isn't person:<handle>"));
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
