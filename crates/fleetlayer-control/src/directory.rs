//! Who belongs where, as decisions need it (`docs/sso.md`, step 1b): a snapshot of every tenant's members (people and
//! agents), teams and roles, and of nodes' owners. Loaded from the tables of step 1a and kept until any of them changes
//! (every change to them queues an audit entry, so the outbox's sequence number says when).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sqlx::Row;

use crate::db::Db;
use crate::people::Owner;

/// A member of a tenant, as its policy sees them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Person {
    pub id: String,
    pub role: String,
    /// The teams they're in directly.
    pub teams: Vec<String>,
}

/// One tenant's members, teams and admitted agents.
#[derive(Debug, Clone, Default)]
pub struct Tenant {
    pub id: String,
    pub slug: String,
    /// By handle.
    pub people: BTreeMap<String, Person>,
    /// Handles by person ID.
    pub handles: HashMap<String, String>,
    /// Each team and its parent.
    pub teams: BTreeMap<String, Option<String>>,
    /// Agent patterns the tenant admitted (`finances@laptop`, `*@laptop`).
    pub agents: Vec<String>,
    /// Services with a `send` token of this tenant (`service:<name>`): members, like its people.
    pub services: Vec<String>,
}

/// The whole deployment's membership.
#[derive(Debug, Clone, Default)]
pub struct Fleet {
    pub tenants: Vec<Tenant>,
    pub node_owners: HashMap<String, Owner>,
}

/// The node a subject or target is on: the part after the last `@` (`tool:browser@laptop` is on `laptop`).
fn node_of(name: &str) -> Option<&str> {
    if is_tenant_thing(name) {
        return None;
    }
    name.rsplit_once('@').map(|(_, n)| n).filter(|n| !n.is_empty())
}

/// Things that belong to a tenant rather than to a node: services, pages, apps and people.
pub fn is_tenant_thing(name: &str) -> bool {
    ["service:", "page:", "app:", "person:"].iter().any(|k| name.starts_with(k))
}

/// Agents, and what's on a node (tools, bundles, terminals): decided in the tenants of whoever's on either side.
pub fn is_agent_side(name: &str) -> bool {
    node_of(name).is_some()
}

impl Tenant {
    /// A team and every team above it: being in Platform puts you in Engineering too.
    pub fn with_parents(&self, team: &str) -> Vec<String> {
        let mut out = vec![];
        let mut cur = Some(team.to_string());
        while let Some(t) = cur {
            if out.contains(&t) {
                break;
            }
            cur = self.teams.get(&t).cloned().flatten();
            out.push(t);
        }
        out
    }

    /// Every team a member is in, directly or through a sub-team.
    pub fn teams_of(&self, handle: &str) -> BTreeSet<String> {
        self.people
            .get(handle)
            .map_or_else(BTreeSet::new, |p| p.teams.iter().flat_map(|t| self.with_parents(t)).collect())
    }

    /// Whether `name` (`person:<handle>`, an agent, or a tool or bundle on a node) is a member here.
    pub fn is_member(&self, fleet: &Fleet, name: &str) -> bool {
        if let Some(h) = name.strip_prefix("person:") {
            return self.people.contains_key(h);
        }
        if name.starts_with("service:") {
            return self.services.iter().any(|s| s == name);
        }
        let Some(node) = node_of(name) else { return false };
        let owner = fleet.node_owners.get(node);
        // A team's or tenant's node: its agents are its tenant's, without being admitted.
        if owner.and_then(|o| o.tenant()) == Some(self.id.as_str()) {
            return true;
        }
        // Tools, bundles and terminals count as their node's.
        let candidates = [name.to_string(), format!("-@{node}")];
        if !self.agents.iter().any(|p| candidates.iter().any(|c| crate::acl::glob(p, c))) {
            return false;
        }
        match owner {
            // A person's agents count only while the person is a member.
            Some(Owner::Person(p)) => self.handles.contains_key(p),
            _ => true,
        }
    }

    /// Who owns a node's things, in this tenant's words: `person:<handle>`, `team:<name>`, `tenant`, or "" (nobody,
    /// or someone outside this tenant).
    pub fn node_owner(&self, fleet: &Fleet, name: &str) -> String {
        let Some(node) = node_of(name) else { return String::new() };
        match fleet.node_owners.get(node) {
            Some(Owner::Person(p)) => self.handles.get(p).map(|h| format!("person:{h}")).unwrap_or_default(),
            Some(Owner::Team { tenant, team }) if *tenant == self.id => format!("team:{team}"),
            Some(Owner::Tenant(t)) if *t == self.id => "tenant".into(),
            _ => String::new(),
        }
    }
}

impl Fleet {
    pub async fn load(db: &Db) -> anyhow::Result<Fleet> {
        let mut tenants: BTreeMap<String, Tenant> = BTreeMap::new();
        for r in sqlx::query("SELECT id, slug FROM tenants ORDER BY created_at, id").fetch_all(db).await? {
            let id: String = r.get(0);
            tenants.insert(id.clone(), Tenant { id, slug: r.get(1), ..Default::default() });
        }
        for r in sqlx::query("SELECT tenant, person, handle, role FROM members WHERE removed_at IS NULL")
            .fetch_all(db)
            .await?
        {
            if let Some(t) = tenants.get_mut(&r.get::<String, _>(0)) {
                let (person, handle): (String, String) = (r.get(1), r.get(2));
                t.handles.insert(person.clone(), handle.clone());
                t.people.insert(handle, Person { id: person, role: r.get(3), teams: vec![] });
            }
        }
        for r in sqlx::query("SELECT tenant, name, parent FROM teams").fetch_all(db).await? {
            if let Some(t) = tenants.get_mut(&r.get::<String, _>(0)) {
                t.teams.insert(r.get(1), r.get(2));
            }
        }
        for r in sqlx::query("SELECT tenant, team, person FROM team_members").fetch_all(db).await? {
            if let Some(t) = tenants.get_mut(&r.get::<String, _>(0))
                && let Some(h) = t.handles.get(&r.get::<String, _>(2)).cloned()
                && let Some(p) = t.people.get_mut(&h)
            {
                p.teams.push(r.get(1));
            }
        }
        for r in sqlx::query("SELECT tenant, pattern FROM member_agents").fetch_all(db).await? {
            if let Some(t) = tenants.get_mut(&r.get::<String, _>(0)) {
                t.agents.push(r.get(1));
            }
        }
        for r in sqlx::query("SELECT tenant, name FROM api_tokens WHERE scope = 'send'").fetch_all(db).await? {
            if let Some(t) = tenants.get_mut(&r.get::<String, _>(0)) {
                t.services.push(format!("service:{}", r.get::<String, _>(1)));
            }
        }
        let mut node_owners = HashMap::new();
        for r in sqlx::query("SELECT name, owner FROM nodes WHERE owner IS NOT NULL").fetch_all(db).await? {
            if let Some(o) = Owner::parse(&r.get::<String, _>(1)) {
                node_owners.insert(r.get(0), o);
            }
        }
        // The deployment's first tenant first, as `tenant_or_first` picks it.
        let mut list: Vec<Tenant> = tenants.into_values().collect();
        let first = sqlx::query("SELECT id FROM tenants ORDER BY created_at, id LIMIT 1")
            .fetch_optional(db)
            .await?
            .map(|r| r.get::<String, _>(0));
        list.sort_by_key(|t| Some(&t.id) != first.as_ref());
        Ok(Fleet { tenants: list, node_owners })
    }

    /// A change marker: it moves whenever a table the snapshot reads changes (they all queue an audit entry).
    pub async fn version(db: &Db) -> anyhow::Result<i64> {
        let seq = sqlx::query("SELECT seq FROM sqlite_sequence WHERE name = 'audit_outbox'")
            .fetch_optional(db)
            .await?
            .map(|r| r.get::<i64, _>(0))
            .unwrap_or(0);
        let nodes: i64 = sqlx::query("SELECT COUNT(*) FROM nodes").fetch_one(db).await?.get(0);
        // Send tokens make services members; adding or replacing one doesn't go through the audit outbox.
        let sends: i64 = sqlx::query(
            "SELECT COALESCE(SUM(created_at % 1000003), 0) + COUNT(*) FROM api_tokens WHERE scope = 'send'",
        )
        .fetch_one(db)
        .await?
        .get(0);
        Ok(seq * 1_000_000 + nodes + sends * 7)
    }

    pub fn tenant(&self, id_or_slug: &str) -> Option<&Tenant> {
        self.tenants.iter().find(|t| t.id == id_or_slug).or_else(|| self.tenants.iter().find(|t| t.slug == id_or_slug))
    }

    /// The tenants `name` is a member of.
    pub fn tenants_of(&self, name: &str) -> Vec<&Tenant> {
        self.tenants.iter().filter(|t| t.is_member(self, name)).collect()
    }

    /// The tenants that decide `who` doing something to `target`: every tenant either side is a member of. Things that
    /// belong to a tenant (services, pages, apps) are decided by the caller's chosen tenant instead.
    pub fn deciders(&self, who: &str, target: &str) -> Vec<&Tenant> {
        let mut out = self.tenants_of(who);
        if is_agent_side(target) {
            for t in self.tenants_of(target) {
                if !out.iter().any(|o| o.id == t.id) {
                    out.push(t);
                }
            }
        }
        out
    }

    /// Whether `name` is a member of a tenant other than `tenant`.
    pub fn elsewhere(&self, tenant: &str, name: &str) -> bool {
        self.tenants.iter().any(|t| t.id != tenant && t.is_member(self, name))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Acme and Home from `docs/sso.md`'s worked example.
    pub fn example() -> Fleet {
        let person = |id: &str, role: &str, teams: &[&str]| Person {
            id: id.into(),
            role: role.into(),
            teams: teams.iter().map(|t| t.to_string()).collect(),
        };
        let mut acme = Tenant { id: "t_acme".into(), slug: "acme".into(), ..Default::default() };
        acme.people.insert("a".into(), person("p_a", "admin", &["everyone", "engineering"]));
        acme.people.insert("c".into(), person("p_c", "admin", &["everyone", "platform"]));
        acme.people.insert("d".into(), person("p_d", "member", &["everyone", "support"]));
        for (team, parent) in
            [("everyone", None), ("engineering", None), ("platform", Some("engineering")), ("support", None)]
        {
            acme.teams.insert(team.into(), parent.map(String::from));
        }
        acme.agents = vec!["billing@laptop".into(), "finances@laptop".into(), "*@c-vm".into()];
        let mut home = Tenant { id: "default".into(), slug: "home".into(), ..Default::default() };
        home.people.insert("a".into(), person("p_a", "admin", &["everyone"]));
        home.people.insert("b".into(), person("p_b", "admin", &["everyone"]));
        home.teams.insert("everyone".into(), None);
        home.agents = vec!["recipes@laptop".into(), "finances@laptop".into()];
        for t in [&mut acme, &mut home] {
            t.handles = t.people.iter().map(|(h, p)| (p.id.clone(), h.clone())).collect();
        }
        let node_owners = HashMap::from([
            ("laptop".to_string(), Owner::Person("p_a".into())),
            ("c-vm".to_string(), Owner::Person("p_c".into())),
            ("build-vm".to_string(), Owner::Team { tenant: "t_acme".into(), team: "engineering".into() }),
        ]);
        Fleet { tenants: vec![home, acme], node_owners }
    }

    #[test]
    fn agents_are_members_where_admitted() {
        let f = example();
        let slugs = |n: &str| f.tenants_of(n).iter().map(|t| t.slug.clone()).collect::<Vec<_>>();
        assert_eq!(slugs("finances@laptop"), ["home", "acme"]);
        assert_eq!(slugs("recipes@laptop"), ["home"]);
        assert_eq!(slugs("billing@laptop"), ["acme"]);
        assert_eq!(slugs("x@build-vm"), ["acme"], "a team's node is its tenant's without being admitted");
        assert_eq!(slugs("tool:shell@build-vm"), ["acme"]);
        assert!(slugs("x@nowhere").is_empty());
        assert_eq!(slugs("person:b"), ["home"]);
        assert!(f.elsewhere("t_acme", "finances@laptop"));
        assert!(!f.elsewhere("t_acme", "billing@laptop"));
    }

    #[test]
    fn a_persons_agents_leave_with_them() {
        let mut f = example();
        let acme = f.tenants.iter_mut().find(|t| t.slug == "acme").unwrap();
        acme.people.remove("c");
        acme.handles.remove("p_c");
        assert!(f.tenants_of("x@c-vm").is_empty());
    }

    #[test]
    fn deciders_are_both_sides_tenants() {
        let f = example();
        let slugs = |w: &str, t: &str| f.deciders(w, t).iter().map(|t| t.slug.clone()).collect::<Vec<_>>();
        assert_eq!(slugs("recipes@laptop", "billing@laptop"), ["home", "acme"]);
        assert_eq!(slugs("billing@laptop", "channel:news"), ["acme"]);
        assert_eq!(slugs("billing@laptop", "service:pages"), ["acme"]);
    }

    #[test]
    fn teams_include_their_sub_teams_and_owners_are_in_the_tenants_words() {
        let f = example();
        let acme = f.tenant("acme").unwrap();
        assert_eq!(acme.teams_of("c").into_iter().collect::<Vec<_>>(), ["engineering", "everyone", "platform"]);
        assert_eq!(acme.node_owner(&f, "billing@laptop"), "person:a");
        assert_eq!(acme.node_owner(&f, "x@build-vm"), "team:engineering");
        assert_eq!(f.tenant("home").unwrap().node_owner(&f, "x@c-vm"), "", "C isn't in Home");
    }
}
