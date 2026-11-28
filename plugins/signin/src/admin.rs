//! The admin pages (`docs/sso.md`, step 2b): members, invites and, under Advanced, teams and the services that sign
//! in here. Simple first: someone with one place to manage goes straight to it, and words like tenant or team appear
//! only under Advanced. The control plane checks every change is the person's to make and records it as theirs; this
//! side asks for a fresh passkey before changes that are hard to undo, limits invites, and checks a CSRF token.

use std::sync::Arc;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::store::{now_ms, sha256_hex};
use crate::web::{App, esc, page, page_wide};

/// A passkey this recent lets someone remove a member, change a role or take someone off a team.
const STEP_UP_MS: i64 = 10 * 60_000;
/// Invites one admin may make in an hour.
const INVITES_PER_HOUR: usize = 20;

fn csrf(cookie_value: &str) -> String {
    sha256_hex(&format!("{cookie_value}:csrf"))
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// `/admin`: straight to the one place this person manages, or a choice.
pub async fn index(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Some(person) = app.signed_in(&headers).await else {
        return Redirect::to("/login?return=%2Fadmin").into_response();
    };
    let view = match app.control.person(&person).await {
        Ok(Some(v)) => v,
        _ => {
            return page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Try again soon",
                "<p>Sign-in can't reach the rest of Fleetlayer right now.</p>",
            );
        }
    };
    let admin_of: Vec<_> = view.tenants.iter().filter(|t| t.role == "admin").collect();
    match admin_of.as_slice() {
        [one] => Redirect::to(&format!("/admin/{}", enc(&one.slug))).into_response(),
        [] => page(StatusCode::FORBIDDEN, "Nothing to manage", "<p>You aren't an admin anywhere here.</p>"),
        many => {
            let links: String = many
                .iter()
                .map(|t| {
                    format!("<li><a class=\"choice\" href=\"/admin/{}\">{}</a></li>", esc(&enc(&t.slug)), esc(&t.name))
                })
                .collect();
            page(StatusCode::OK, "Manage", &format!("<ul class=\"choices\">{links}</ul>"))
        }
    }
}

#[derive(Deserialize)]
pub struct ShowQuery {
    msg: Option<String>,
    invite: Option<String>,
}

pub async fn show(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(tenant): Path<String>,
    Query(q): Query<ShowQuery>,
) -> Response {
    let Some((session, cookie)) = app.session(&headers).await else {
        return Redirect::to(&format!("/login?return={}", enc(&format!("/admin/{tenant}")))).into_response();
    };
    let v = match app.control.admin_view(&tenant, &session.person).await {
        Ok(v) => v,
        Err(e) => {
            return page(
                StatusCode::FORBIDDEN,
                "Can't manage this",
                &format!("<p class=\"why\">{}</p>", esc(&e.to_string())),
            );
        }
    };
    let token = csrf(&cookie);
    let form = |action: &str, fields: &[(&str, &str)], button: &str, class: &str, confirm: Option<&str>| {
        let hidden: String = fields
            .iter()
            .map(|(k, v)| format!("<input type=\"hidden\" name=\"{}\" value=\"{}\">", esc(k), esc(v)))
            .collect();
        let onclick = confirm.map(|c| format!(" onclick=\"return confirm('{}')\"", esc(c))).unwrap_or_default();
        format!(
            "<form method=\"post\"><input type=\"hidden\" name=\"csrf\" value=\"{token}\"><input type=\"hidden\" \
             name=\"action\" value=\"{action}\">{hidden}<button class=\"btn btn-sm {class}\"{onclick}>{button}</button></form>"
        )
    };
    let me = v["members"]
        .as_array()
        .and_then(|ms| ms.iter().find(|m| m["person"] == session.person.as_str()))
        .and_then(|m| m["handle"].as_str())
        .unwrap_or("")
        .to_string();
    let mut body = String::new();
    if let Some(m) = &q.msg {
        body.push_str(&format!("<p class=\"notice is-info\" role=\"status\">{}</p>", esc(m)));
    }
    if let Some(code) = &q.invite {
        let link = format!("{}/invite?code={}", app.settings.issuer, enc(code));
        body.push_str(&format!(
            "<div class=\"panel\"><h2>Invite link ready</h2><p>Send this link to them. It works once, for a \
             week.</p><p><code>{}</code></p></div>",
            esc(&link)
        ));
    }
    // Members.
    body.push_str("<section class=\"section\"><h2>People</h2><div class=\"list\">");
    for m in v["members"].as_array().into_iter().flatten() {
        let (name, handle, role) = (
            m["display_name"].as_str().unwrap_or(""),
            m["handle"].as_str().unwrap_or(""),
            m["role"].as_str().unwrap_or(""),
        );
        let other = if role == "admin" { "member" } else { "admin" };
        let you = if handle == me { " <span class=\"tag\">you</span>" } else { "" };
        let role_tag = if role == "admin" { "<span class=\"tag is-accent\">admin</span>" } else { "" };
        body.push_str(&format!(
            "<div class=\"row\"><div class=\"who\"><div class=\"name\">{}{you} {role_tag}</div><div class=\"meta\">{}</div></div>\
             <div class=\"tools\">{}{}</div></div>",
            esc(name),
            esc(handle),
            form("member_role", &[("handle", handle), ("role", other)], &format!("Make {other}"), "", None),
            form("member_remove", &[("handle", handle)], "Remove", "btn-quiet-danger", Some(&format!("Remove {name}?")))
        ));
    }
    body.push_str("</div></section>");
    // Invites.
    body.push_str(&format!(
        "<section class=\"section\"><h2>Invite someone</h2><div class=\"panel\"><form class=\"inline-form\" method=\"post\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{token}\"><input type=\"hidden\" name=\"action\" value=\"invite\">\
         <label class=\"label\">Handle (what rules call them)<input name=\"handle\" required \
         pattern=\"[a-z0-9][a-z0-9._-]{{0,63}}\" placeholder=\"sam\"></label><label class=\"label\">Role \
         <select name=\"role\"><option>member</option><option>admin</option></select></label>\
         <button class=\"btn btn-primary\">Make an invite link</button></form></div>"
    ));
    let invites: Vec<&Value> = v["invites"].as_array().into_iter().flatten().collect();
    if !invites.is_empty() {
        body.push_str("<h3 class=\"hint\">Waiting to be used</h3><div class=\"list\">");
        for i in invites {
            let handle = i["handle"].as_str().unwrap_or("");
            body.push_str(&format!(
                "<div class=\"row\"><div class=\"who\"><div class=\"name\">{}</div><div class=\"meta\">{}</div></div>\
                 <div class=\"tools\">{}</div></div>",
                esc(handle),
                esc(i["role"].as_str().unwrap_or("")),
                form("invite_revoke", &[("handle", handle)], "Cancel", "", None)
            ));
        }
        body.push_str("</div>");
    }
    body.push_str("</section>");
    // Advanced: teams and the services that sign in here.
    body.push_str("<details><summary>Advanced</summary><section class=\"section\"><h2>Teams</h2>");
    let teams: Vec<&Value> =
        v["teams"].as_array().into_iter().flatten().filter(|t| t["name"] != crate::EVERYONE).collect();
    if teams.is_empty() {
        body.push_str("<p class=\"hint\">Everyone is in one group. Add a team when you need a second.</p>");
    }
    for t in &teams {
        let name = t["name"].as_str().unwrap_or("");
        let parent =
            t["parent"].as_str().map(|p| format!(" <span class=\"hint\">in {}</span>", esc(p))).unwrap_or_default();
        body.push_str(&format!("<h3>{}{parent}</h3><div class=\"list\">", esc(name)));
        let members: Vec<&Value> = t["members"].as_array().into_iter().flatten().collect();
        if members.is_empty() {
            body.push_str("<div class=\"row empty\">Nobody yet.</div>");
        }
        for m in members {
            let handle = m[0].as_str().unwrap_or("");
            // Put there by a provider's group: taking them off lasts until their groups are read again.
            let (from, confirm) = match m[2].as_str() {
                Some(u) => (
                    format!(" <span class=\"tag\">from {}</span>", esc(u)),
                    Some(format!(
                        "{handle} is in {name} through {u}'s groups: they'll be back in it when those are read again, unless the mapping changes. Take them off anyway?"
                    )),
                ),
                None => (String::new(), None),
            };
            body.push_str(&format!(
                "<div class=\"row\"><div class=\"who\"><div class=\"name\">{}{from}</div></div><div class=\"tools\">{}</div></div>",
                esc(handle),
                form(
                    "team_member",
                    &[("team", name), ("handle", handle), ("add", "false")],
                    "Take off",
                    "",
                    confirm.as_deref()
                )
            ));
        }
        body.push_str("</div>");
    }
    let team_options: String =
        teams.iter().filter_map(|t| t["name"].as_str()).map(|n| format!("<option>{}</option>", esc(n))).collect();
    body.push_str(&format!(
        "<div class=\"panel\"><form class=\"inline-form\" method=\"post\"><input type=\"hidden\" name=\"csrf\" value=\"{token}\">\
         <input type=\"hidden\" name=\"action\" value=\"team_add\"><label class=\"label\">New team <input name=\"team\" required></label>\
         <label class=\"label\">Inside <select name=\"parent\"><option value=\"\">(none)</option>{team_options}</select></label>\
         <button class=\"btn\">Add team</button></form>"
    ));
    if !teams.is_empty() {
        body.push_str(&format!(
            "<form class=\"inline-form\" method=\"post\"><input type=\"hidden\" name=\"csrf\" value=\"{token}\"><input type=\"hidden\" \
             name=\"action\" value=\"team_member\"><input type=\"hidden\" name=\"add\" value=\"true\"><label class=\"label\">Put \
             <input name=\"handle\" required placeholder=\"handle\"></label><label class=\"label\">in <select name=\"team\">{team_options}</select></label> \
             <button class=\"btn\">Add to team</button></form>"
        ));
    }
    body.push_str("</div></section><section class=\"section\"><h2>Services that sign in here</h2><div class=\"list\">");
    for c in &app.settings.clients {
        body.push_str(&format!(
            "<div class=\"row\"><div class=\"who\"><div class=\"name\">{}</div><div class=\"meta\">{}</div></div></div>",
            esc(&c.name),
            esc(&c.redirect_uris.join(", "))
        ));
    }
    body.push_str(
        "</div><p class=\"hint\">Services are set in the sign-in service's settings.</p></section></details>",
    );
    let title = v["name"].as_str().unwrap_or("Manage").to_string();
    page_wide(StatusCode::OK, &title, &body)
}

#[derive(Deserialize)]
pub struct ChangeForm {
    csrf: String,
    action: String,
    #[serde(default)]
    handle: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    add: Option<String>,
}

pub async fn change(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(tenant): Path<String>,
    Form(f): Form<ChangeForm>,
) -> Response {
    let back = |q: &str| Redirect::to(&format!("/admin/{}?{q}", enc(&tenant))).into_response();
    let Some((session, cookie)) = app.session(&headers).await else {
        return Redirect::to("/login").into_response();
    };
    let want = csrf(&cookie);
    if !bool::from(subtle::ConstantTimeEq::ct_eq(want.as_bytes(), f.csrf.as_bytes())) {
        return page(
            StatusCode::FORBIDDEN,
            "Try again",
            "<p>That form is out of date: reload the page and try again.</p>",
        );
    }
    let removing = f.action == "member_remove"
        || f.action == "member_role"
        || (f.action == "team_member" && f.add.as_deref() == Some("false"));
    if removing && now_ms() - session.verified_at > STEP_UP_MS {
        // Hard to undo: prove it's them with a passkey first, then come back to the page.
        let ret = enc(&format!("/admin/{tenant}?msg={}", enc("Signed in again. Now make the change.")));
        return Redirect::to(&format!("/login?return={ret}")).into_response();
    }
    if f.action == "invite" {
        let mut made = app.invites_made.lock().await;
        let mine = made.entry(session.person.clone()).or_default();
        let hour_ago = now_ms() - 3_600_000;
        mine.retain(|t| *t > hour_ago);
        if mine.len() >= INVITES_PER_HOUR {
            return back(&format!("msg={}", enc("That's 20 invites in the last hour, the limit. Try again later.")));
        }
        mine.push(now_ms());
    }
    let body = json!({
        "tenant": tenant, "as": session.person, "handle": f.handle, "role": f.role, "team": f.team,
        "parent": f.parent, "add": f.add.as_deref().map(|a| a != "false"),
    });
    match app.control.admin(&f.action, body).await {
        Ok(out) => match out["code"].as_str() {
            Some(code) => back(&format!("invite={}", enc(code))),
            None => back(&format!("msg={}", enc("Done."))),
        },
        // The control plane's reason, such as the last admin being unable to leave.
        Err(e) => {
            let reason = e.to_string();
            let reason = reason.split_once(": ").map(|(_, r)| r).unwrap_or(&reason).to_string();
            back(&format!("msg={}", enc(&reason)))
        }
    }
}
