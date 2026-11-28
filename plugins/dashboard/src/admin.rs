//! Managing the fleet's people: who's in it and their role, invites, teams, and the access rules in force. Read and
//! changed only through the control plane's API (`/v1/people`, `/v1/policy`), as the signed-in person: the control
//! plane checks they may (one of the tenant's admins, or `admin` on `service:dashboard`) and records every change as
//! theirs. Changes that are hard to undo (removing someone, changing a role, taking someone off a team) ask for their
//! passkey again when they last used it more than ten minutes ago.
//!
//! Plain pages, nothing hidden under "Advanced": teams have a page of their own, which says what teams are for when
//! there are none yet.

use std::fmt::Write as _;

use serde_json::Value;

use crate::icons::icon;
use crate::views::{esc, sec_head, tile};

/// How recent a passkey sign-in must be for a change that's hard to undo.
pub const FRESH_SECS: i64 = 10 * 60;
/// Invites one person may make in an hour.
pub const INVITES_PER_HOUR: usize = 20;

/// The tenant's own team, which everyone is in: never listed or offered.
const EVERYONE: &str = "everyone";

/// The changes that need a recent passkey.
pub fn hard_to_undo(action: &str, add: Option<bool>) -> bool {
    matches!(action, "member_remove" | "member_role" | "team_remove") || (action == "team_member" && add == Some(false))
}

/// What the forms on these pages need.
pub struct Forms<'a> {
    pub csrf: &'a str,
    /// The page the forms are on, to come back to.
    pub here: &'a str,
}

impl Forms<'_> {
    /// A one-button form posting `action` with hidden `fields`.
    fn button(&self, action: &str, fields: &[(&str, &str)], label: &str, class: &str) -> String {
        let hidden: String = fields
            .iter()
            .map(|(k, v)| format!(r#"<input type="hidden" name="{}" value="{}">"#, esc(k), esc(v)))
            .collect();
        format!(
            r#"<form class="row-actions" method="post" action="/_/people/{action}"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="back" value="{}">{hidden}<button class="btn btn-sm{}">{}</button></form>"#,
            esc(self.csrf),
            esc(self.here),
            if class.is_empty() { String::new() } else { format!(" {class}") },
            label
        )
    }

    fn open(&self, action: &str) -> String {
        format!(
            r#"<form class="add-form" method="post" action="/_/people/{action}"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="back" value="{}">"#,
            esc(self.csrf),
            esc(self.here)
        )
    }
}

fn page_head(title: &str, summary: &str) -> String {
    format!(r#"<div class="page-head"><h1>{}</h1><p class="summary">{}</p></div>"#, esc(title), summary)
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// The page when the control plane won't show this person the tenant's people (or can't be reached).
pub fn refused(title: &str, why: &str) -> String {
    format!(
        r#"{}<div class="list"><div class="empty"><span class="empty-icon">{}</span><h3>Only the fleet's admins manage this</h3><p>{}</p></div></div>"#,
        page_head(title, ""),
        icon("shield-check"),
        esc(why)
    )
}

/// `/people`: everyone in the fleet, inviting someone, and the invites not used yet.
pub fn people(v: &Value, f: &Forms, me: Option<&str>, invite_link: Option<&str>) -> String {
    let members: Vec<&Value> = v["members"].as_array().into_iter().flatten().collect();
    let admins = members.iter().filter(|m| s(m, "role") == "admin").count();
    let mut o = page_head(
        "People",
        &esc(&format!(
            "{} in {}, {} of them {}. Each signs in with their own passkey.",
            plural(members.len(), "person", "people"),
            s(v, "name"),
            admins,
            if admins == 1 { "an admin" } else { "admins" }
        )),
    );
    if let Some(link) = invite_link {
        let _ = write!(
            o,
            r#"<div class="notice is-ok" role="status">{}<div><p class="notice-text"><strong>Invite link ready.</strong> Send it to them: it works once, for a week.</p><p><code class="invite-link">{}</code></p></div></div>"#,
            icon("link"),
            esc(link)
        );
    }

    // Invite someone.
    o.push_str(r#"<section aria-labelledby="invite-h">"#);
    o.push_str(&sec_head("invite-h", "Invite someone", None, None));
    let _ = write!(
        o,
        r#"<div class="list"><div class="row">{}{}<div class="row-body form-fields"><label class="field"><span class="label">Their handle</span><input class="input" name="handle" required pattern="[a-z0-9][a-z0-9._-]{{0,63}}" placeholder="sam" autocomplete="off" spellcheck="false"><span class="sub">What rules call them: person:sam.</span></label><label class="field"><span class="label">Role</span><select class="input" name="role"><option value="member">Member</option><option value="admin">Admin</option></select><span class="sub">Admins manage people and approve requests.</span></label></div><div class="row-end"><button class="btn btn-primary">{}Make an invite link</button></div></form></div></div>"#,
        tile("mail-plus", "accent"),
        f.open("invite"),
        icon("plus")
    );
    o.push_str("</section>");

    // Everyone.
    o.push_str(r#"<section aria-labelledby="members-h">"#);
    o.push_str(&sec_head("members-h", "Everyone", Some(members.len()), None));
    o.push_str(r#"<div class="list">"#);
    for m in &members {
        let (name, handle, role) = (s(m, "display_name"), s(m, "handle"), s(m, "role"));
        let you = me == Some(s(m, "person"));
        let other = if role == "admin" { "member" } else { "admin" };
        let teams: Vec<&str> =
            m["teams"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|t| *t != EVERYONE).collect();
        let mut sub = format!(r#"<span class="addr">person:{}</span>"#, esc(handle));
        if !teams.is_empty() {
            let _ = write!(sub, " · in {}", esc(&teams.join(", ")));
        }
        let actions = if you {
            String::new()
        } else {
            format!(
                "{}{}",
                f.button(
                    "member_role",
                    &[("handle", handle), ("role", other)],
                    if other == "admin" { "Make admin" } else { "Make member" },
                    ""
                ),
                f.button("member_remove", &[("handle", handle)], &format!("{}Remove", icon("user-minus")), "btn-ghost")
            )
        };
        let _ = write!(
            o,
            r#"<div class="row">{}<div class="row-body"><p class="row-title">{}{}{}</p><p class="row-sub">{sub}</p></div><div class="row-end row-end-inline">{actions}</div></div>"#,
            tile("user", if role == "admin" { "accent" } else { "" }),
            esc(if name.is_empty() { handle } else { name }),
            if role == "admin" { r#" <span class="tag">admin</span>"# } else { "" },
            if you { r#" <span class="tag">you</span>"# } else { "" },
        );
    }
    o.push_str("</div></section>");

    // Invites not used yet.
    let invites: Vec<&Value> = v["invites"].as_array().into_iter().flatten().collect();
    if !invites.is_empty() {
        o.push_str(r#"<section aria-labelledby="invites-h">"#);
        o.push_str(&sec_head("invites-h", "Invites not used yet", Some(invites.len()), None));
        o.push_str(r#"<div class="list">"#);
        for i in invites {
            let handle = s(i, "handle");
            let expires = i["expires_at"].as_i64().map(crate::views::until_words).unwrap_or_default();
            let _ = write!(
                o,
                r#"<div class="row">{}<div class="row-body"><p class="row-title"><span class="addr">person:{}</span> <span class="tag">{}</span></p><p class="row-sub">Made by {}. Works until {}.</p></div><div class="row-end row-end-inline">{}</div></div>"#,
                tile("link", ""),
                esc(handle),
                esc(s(i, "role")),
                esc(s(i, "created_by")),
                esc(&expires),
                f.button("invite_revoke", &[("handle", handle)], "Cancel it", "btn-ghost")
            );
        }
        o.push_str("</div></section>");
    }
    o
}

/// `/teams`: the teams, who's in each, and making and filling them.
pub fn teams(v: &Value, f: &Forms) -> String {
    let teams: Vec<&Value> = v["teams"].as_array().into_iter().flatten().filter(|t| s(t, "name") != EVERYONE).collect();
    let mut o = page_head(
        "Teams",
        "Groups of people the access rules can name (team:design), each inside another if you like. Everyone is in the fleet's own group already.",
    );
    let handles: Vec<&str> = v["members"].as_array().into_iter().flatten().map(|m| s(m, "handle")).collect();
    let options = |names: &[&str]| -> String {
        names.iter().map(|n| format!(r#"<option value="{0}">{0}</option>"#, esc(n))).collect()
    };
    let team_names: Vec<&str> = teams.iter().map(|t| s(t, "name")).collect();

    // Make one, and put someone in one.
    o.push_str(r#"<section aria-labelledby="new-h">"#);
    o.push_str(&sec_head("new-h", "Make a team", None, None));
    let _ = write!(
        o,
        r#"<div class="list"><div class="row">{}{}<div class="row-body form-fields"><label class="field"><span class="label">Name</span><input class="input" name="team" required pattern="[a-z0-9][a-z0-9._-]{{0,63}}" placeholder="design" autocomplete="off" spellcheck="false"></label><label class="field"><span class="label">Inside</span><select class="input" name="parent"><option value="">No other team</option>{}</select></label></div><div class="row-end"><button class="btn btn-primary">{}Make the team</button></div></form></div>"#,
        tile("users-group", "accent"),
        f.open("team_add"),
        options(&team_names),
        icon("plus")
    );
    if !teams.is_empty() && !handles.is_empty() {
        let _ = write!(
            o,
            r#"<div class="row">{}{}<input type="hidden" name="add" value="true"><div class="row-body form-fields"><label class="field"><span class="label">Put</span><select class="input" name="handle">{}</select></label><label class="field"><span class="label">in</span><select class="input" name="team">{}</select></label></div><div class="row-end"><button class="btn">Add to the team</button></div></form></div>"#,
            tile("user", ""),
            f.open("team_member"),
            options(&handles),
            options(&team_names)
        );
    }
    o.push_str("</div></section>");

    if teams.is_empty() {
        let _ = write!(
            o,
            r#"<section><div class="list"><div class="empty"><span class="empty-icon">{}</span><h3>No teams yet</h3><p>Make one when some people need different access from the rest: a rule can then say <code>team:design</code> instead of naming each person.</p></div></div></section>"#,
            icon("users-group")
        );
    }
    for t in teams {
        let name = s(t, "name");
        let id = format!("t-{}", name.replace(|c: char| !c.is_ascii_alphanumeric(), "-"));
        let parent = t["parent"].as_str().map(|p| format!("Inside {p}.")).unwrap_or_default();
        let members: Vec<&Value> = t["members"].as_array().into_iter().flatten().collect();
        let _ = write!(
            o,
            r#"<section aria-labelledby="{id}"><div class="sec-head"><h2 id="{id}">{} <span class="count">{}</span></h2>{}</div>"#,
            esc(name),
            members.len(),
            f.button("team_remove", &[("team", name)], &format!("{}Remove the team", icon("trash")), "btn-ghost"),
        );
        if !parent.is_empty() {
            let _ = write!(o, r#"<p class="hint">{}</p>"#, esc(&parent));
        }
        o.push_str(r#"<div class="list">"#);
        if members.is_empty() {
            o.push_str(r#"<div class="row"><div class="row-body"><p class="row-sub">Nobody yet.</p></div></div>"#);
        }
        for m in members {
            let handle = m[0].as_str().unwrap_or("");
            let maintainer = m[1].as_str() == Some("maintainer");
            let from =
                m[2].as_str().map(|u| format!(r#" <span class="tag">from {}</span>"#, esc(u))).unwrap_or_default();
            let _ = write!(
                o,
                r#"<div class="row">{}<div class="row-body"><p class="row-title"><span class="addr">person:{}</span>{}{from}</p>{}</div><div class="row-end row-end-inline">{}</div></div>"#,
                tile("user", ""),
                esc(handle),
                if maintainer { r#" <span class="tag">maintainer</span>"# } else { "" },
                if from.is_empty() {
                    String::new()
                } else {
                    r#"<p class="row-sub">In it through their sign-in provider's groups: taken off, they're back when those are read again.</p>"#.to_string()
                },
                f.button(
                    "team_member",
                    &[("team", name), ("handle", handle), ("add", "false")],
                    "Take off",
                    "btn-ghost"
                )
            );
        }
        o.push_str("</div></section>");
    }
    o
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn list_of(v: &Value) -> String {
    match v {
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
        Value::String(s) => s.clone(),
        Value::Null => "anything".into(),
        other => other.to_string(),
    }
}

fn rule_rows(rules: &[Value], kind: &str) -> String {
    let mut o = String::new();
    for r in rules {
        let to = if r["to"].is_null() { "anything".to_string() } else { list_of(&r["to"]) };
        let mut sub = vec![];
        if let Some(e) = r["except"].as_array().filter(|e| !e.is_empty()) {
            sub.push(format!("except {}", e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
        }
        if let Some(u) = r["until"].as_str() {
            sub.push(format!("until {u}"));
        }
        if let Some(w) = r["why"].as_str() {
            sub.push(w.to_string());
        }
        let _ = write!(
            o,
            r#"<div class="row">{}<div class="row-body"><p class="row-title"><span class="addr">{}</span> {} <strong>{}</strong> {} <span class="addr">{}</span></p>{}</div></div>"#,
            tile(if kind == "deny" { "trash" } else { "shield-check" }, if kind == "deny" { "bad" } else { "" }),
            esc(&list_of(&r["who"])),
            match kind {
                "deny" => "may never",
                "only" => "may only",
                _ => "may",
            },
            esc(&list_of(&r["can"])),
            if to == "anything" { "" } else { "on" },
            esc(&to),
            if sub.is_empty() {
                String::new()
            } else {
                format!(r#"<p class="row-sub">{}</p>"#, esc(&sub.join(" · ")))
            }
        );
    }
    o
}

/// `/rules`: the access policy in force, read-only: who may do what, the roles, and the file itself.
pub fn rules(v: &Value) -> String {
    let mut o = page_head(
        "Access rules",
        "Who may do what. Read here; changed in the policy file, kept in git where the agents it limits can't change it, and applied with <code>fleetlayer-control acl apply</code>.",
    );
    let Some(p) = v["policy"].as_object() else {
        let _ = write!(
            o,
            r#"<div class="list"><div class="empty"><span class="empty-icon">{}</span><h3>No policy file</h3><p>This fleet still uses the older message rules (<code>fleetlayer-control acl</code>). A policy file says more: “Access control” in the docs.</p></div></div>"#,
            icon("shield-check")
        );
        return o;
    };
    if v["builtin"].as_bool() == Some(true) {
        o.push_str(r#"<div class="notice is-ok">"#);
        o.push_str(&icon("shield-check"));
        o.push_str(r#"<p class="notice-text">These are the built-in rules every new fleet starts with. Write your own policy file when you need different ones.</p></div>"#);
    }
    let default = p.get("default").and_then(Value::as_str).unwrap_or("deny");
    let _ = write!(
        o,
        r#"<p class="hint">When no rule says, agents are {}.</p>"#,
        if default == "allow" { "allowed" } else { "refused" }
    );
    for (key, title, kind) in [("deny", "Never", "deny"), ("only", "Only", "only"), ("allow", "Allowed", "allow")] {
        let rules: Vec<Value> = p.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
        if rules.is_empty() {
            continue;
        }
        let id = format!("rules-{key}");
        let _ = write!(
            o,
            r#"<section aria-labelledby="{id}">{}<div class="list">{}</div></section>"#,
            sec_head(&id, title, Some(rules.len()), None),
            rule_rows(&rules, kind)
        );
    }
    if let Some(roles) = p.get("roles").and_then(Value::as_object).filter(|r| !r.is_empty()) {
        o.push_str(r#"<section aria-labelledby="roles-h">"#);
        o.push_str(&sec_head("roles-h", "Agent roles", Some(roles.len()), None));
        o.push_str(r#"<div class="list">"#);
        for (name, r) in roles {
            let windowed = r["activation"].as_str() == Some("window");
            let _ = write!(
                o,
                r#"<div class="row">{}<div class="row-body"><p class="row-title"><span class="addr">{}</span>{}</p><p class="row-sub">Its agents: <span class="addr">{}</span>{}</p></div></div>"#,
                tile("lock-open", if windowed { "accent" } else { "" }),
                esc(name),
                if windowed { r#" <span class="tag">window</span>"# } else { "" },
                esc(&list_of(&r["who"])),
                r["why"].as_str().map(|w| format!(". {}", esc(w))).unwrap_or_default()
            );
        }
        o.push_str("</div></section>");
    }
    if let Some(text) = v["text"].as_str() {
        o.push_str(r#"<section aria-labelledby="file-h">"#);
        o.push_str(&sec_head("file-h", "The policy file", None, None));
        let _ =
            write!(o, r#"<div class="list"><pre class="policy-text"><code>{}</code></pre></div></section>"#, esc(text));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn forms() -> Forms<'static> {
        Forms { csrf: "tok", here: "/people" }
    }

    fn tenant() -> Value {
        json!({"name": "Home", "members": [
            {"person": "p_sam", "display_name": "Sam", "handle": "sam", "role": "admin", "teams": ["everyone", "design"]},
            {"person": "p_lee", "display_name": "<b>Lee</b>", "handle": "lee", "role": "member", "teams": ["everyone"]}],
         "invites": [{"handle": "kim", "role": "member", "created_by": "person:sam (via dashboard)", "expires_at": 4_102_444_800_000_i64}],
         "teams": [{"name": "everyone", "members": []},
                   {"name": "design", "parent": null, "members": [["sam", "member", null], ["lee", "member", "google"]]}]})
    }

    #[test]
    fn people_lists_everyone_with_their_actions_but_none_on_yourself() {
        let o = people(&tenant(), &forms(), Some("p_sam"), Some("https://signin.example/invite?code=abc"));
        assert!(o.contains("2 people in Home, 1 of them an admin"), "{o}");
        assert!(o.contains("invite?code=abc"));
        assert!(o.contains("&lt;b&gt;Lee&lt;/b&gt;") && !o.contains("<b>Lee</b>"), "escaped");
        assert!(
            o.contains(r#"name="handle" value="lee""#) && !o.contains(r#"name="handle" value="sam""#),
            "no buttons on yourself: {o}"
        );
        assert!(o.contains("in design") && !o.contains("in everyone"));
        assert!(o.contains("person:kim") && o.contains("invite_revoke"));
        assert!(!o.contains("Advanced"));
    }

    #[test]
    fn teams_are_a_plain_page_and_say_what_they_re_for_when_there_are_none() {
        let o = teams(&tenant(), &forms());
        assert!(o.contains("design") && o.contains("from google") && o.contains("Take off"), "{o}");
        assert!(!o.contains(">everyone<"));
        let mut none = tenant();
        none["teams"] = json!([{"name": "everyone", "members": []}]);
        let o = teams(&none, &forms());
        assert!(o.contains("No teams yet") && o.contains("team:design"), "{o}");
    }

    #[test]
    fn rules_read_like_sentences() {
        let v = json!({"text": "version: 2\n", "builtin": false, "policy": {"default": "deny",
            "deny": [{"who": "*@triage", "can": "message", "to": ["*@agents"], "why": "it reads strangers' email"}],
            "allow": [{"who": ["team:design"], "can": ["sign_in"], "to": ["service:dashboard"]}],
            "roles": {"admin": {"who": "*@admin", "activation": "window"}}}});
        let o = rules(&v);
        assert!(o.contains("may never") && o.contains("message") && o.contains("strangers"), "{o}");
        assert!(o.contains("team:design") && o.contains("service:dashboard"));
        assert!(o.contains(">window<") && o.contains("version: 2"));
        assert!(rules(&json!({"text": null, "policy": null})).contains("No policy file"));
    }

    #[test]
    fn what_needs_a_fresh_passkey() {
        assert!(hard_to_undo("member_remove", None) && hard_to_undo("member_role", None));
        assert!(hard_to_undo("team_member", Some(false)) && !hard_to_undo("team_member", Some(true)));
        assert!(!hard_to_undo("invite", None) && !hard_to_undo("team_add", None));
    }
}
