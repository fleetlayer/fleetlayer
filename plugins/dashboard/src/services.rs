//! `/services`: every web service the fleet runs or uses, with its link, where it runs and whether it's healthy. Built
//! from what Fleetlayer already knows (`Snapshot::services`: each gateway's routes, the services node plugins
//! announce, the sign-in service's clients) and the links admins add here. Each viewer sees only the services the
//! access policy lets them use (`sign_in` or `use` on `service:<name>`); web.rs filters before this renders.

use std::fmt::Write as _;

use crate::icons::icon;
use crate::model::{Health, Service, Snapshot};
use crate::views::{addr, dot, empty, errors, esc, page_head, sec_head, tile};

/// What the page needs besides the services.
pub struct Ctx<'a> {
    /// Whether the viewer may add and remove links (the control plane checks they're one of the tenant's admins).
    pub can_manage: bool,
    pub csrf: &'a str,
}

fn health(h: &Health) -> String {
    match h {
        Health::Up => format!(r#"{}<span class="sub">Up</span>"#, dot("ok", "Up")),
        Health::Down(why) => {
            format!(r#"{}<span class="sub is-bad" title="{}">Down</span>"#, dot("bad", "Down"), esc(why))
        }
        Health::Unchecked => r#"<span class="sub">Not checked</span>"#.to_string(),
    }
}

fn icon_for(s: &Service) -> &'static str {
    match s.name.as_str() {
        "signin" => "key",
        "dashboard" | "fleetlayer" => "gauge",
        "hub" => "users-group",
        "pages" => "list-details",
        "control" => "topology-star-3",
        _ if s.link => "link",
        _ => "server",
    }
}

fn row(s: &Service, c: &Ctx) -> String {
    let title = match &s.address {
        Some(a) => format!(
            r#"<a class="service-link" href="{}" target="_blank" rel="noopener noreferrer">{}</a>"#,
            esc(a),
            esc(&s.name)
        ),
        None => esc(&s.name),
    };
    let mut sub: Vec<String> = vec![];
    if let Some(a) = &s.address {
        sub.push(addr(a.trim_start_matches("https://").trim_start_matches("http://")));
    }
    match (&s.machine, &s.plugin) {
        (Some(m), Some(p)) => sub.push(format!("on {} ({})", esc(m), esc(p))),
        (Some(m), None) => sub.push(format!("on {}", esc(m))),
        (None, Some(p)) => sub.push(esc(p)),
        (None, None) if s.link => sub.push("added here".into()),
        (None, None) => {}
    }
    if s.address.is_none() && s.node.is_some() {
        sub.push("no address yet: no gateway routes it".into());
    }
    let mut tags = String::new();
    if s.signs_in {
        tags.push_str(r#" <span class="tag">Fleetlayer sign-in</span>"#);
    }
    if s.link {
        tags.push_str(r#" <span class="tag">link</span>"#);
    }
    let remove = if s.link && c.can_manage {
        format!(
            r#"<form class="row-actions" method="post" action="/_/links/{}/remove"><input type="hidden" name="csrf" value="{}"><button class="btn btn-sm btn-ghost">{}Remove</button></form>"#,
            esc(&form_urlencoded::byte_serialize(s.name.as_bytes()).collect::<String>()),
            esc(c.csrf),
            icon("trash")
        )
    } else {
        String::new()
    };
    format!(
        r#"<div class="row">{}<div class="row-body"><p class="row-title">{title}{tags}</p><p class="row-sub">{}</p>{}</div><div class="row-end row-end-inline">{}{remove}</div></div>"#,
        tile(icon_for(s), if matches!(s.health, Health::Down(_)) { "bad" } else { "" }),
        sub.join(" · "),
        s.description.as_deref().map(|d| format!(r#"<p class="row-sub">{}</p>"#, esc(d))).unwrap_or_default(),
        if s.link { String::new() } else { health(&s.health) },
    )
}

/// The page, for the services this viewer may use.
pub fn page(snap: &Snapshot, services: &[Service], c: &Ctx) -> String {
    let mut o = errors(snap);
    let down = services.iter().filter(|s| matches!(s.health, Health::Down(_))).count();
    let summary = match (services.len(), down) {
        (0, _) => String::new(),
        (n, 0) => format!("{n} web service{} you can open, all answering.", if n == 1 { "" } else { "s" }),
        (n, d) => format!("{n} web service{} you can open; {d} not answering.", if n == 1 { "" } else { "s" }),
    };
    o.push_str(&page_head("Services", &esc(&summary)));

    // Run by the fleet: a node runs it. Elsewhere: services that only sign in with Fleetlayer, and links added here.
    let (run, links): (Vec<&Service>, Vec<&Service>) = services.iter().partition(|s| s.node.is_some());
    o.push_str(r#"<section aria-labelledby="run-h">"#);
    o.push_str(&sec_head("run-h", "Run by the fleet", Some(run.len()), None));
    o.push_str(r#"<div class="list">"#);
    for s in &run {
        o.push_str(&row(s, c));
    }
    if run.is_empty() {
        o.push_str(&empty(
            "server",
            "No services yet",
            "Pages, the Claude hub, sign-in and every service a gateway routes show here, once a node reports them.",
        ));
    }
    o.push_str("</div></section>");

    if !links.is_empty() || c.can_manage {
        o.push_str(r#"<section aria-labelledby="links-h">"#);
        o.push_str(&sec_head("links-h", "Elsewhere", Some(links.len()), None));
        o.push_str(r#"<div class="list">"#);
        for s in &links {
            o.push_str(&row(s, c));
        }
        if c.can_manage {
            let _ = write!(
                o,
                r#"<div class="row">{}<form class="add-form" method="post" action="/_/links"><input type="hidden" name="csrf" value="{}"><div class="row-body form-fields"><label class="field"><span class="label">Name</span><input class="input" name="name" required pattern="[a-z0-9][a-z0-9-]{{0,62}}" placeholder="wiki" autocomplete="off" spellcheck="false"><span class="sub">The policy calls it service:wiki.</span></label><label class="field"><span class="label">Address</span><input class="input" name="url" type="url" required placeholder="https://wiki.example.com"></label><label class="field"><span class="label">What it is</span><input class="input" name="description" maxlength="200" placeholder="The team's handbook"></label></div><div class="row-end"><button class="btn btn-primary">{}Add the link</button></div></form></div>"#,
                tile("link", "accent"),
                esc(c.csrf),
                icon("plus")
            );
        }
        o.push_str("</div>");
        if c.can_manage {
            o.push_str(r#"<p class="hint">Add a link for a service Fleetlayer doesn't run. Who sees it is up to the access policy, like any service: a rule giving <code>sign_in</code> or <code>use</code> on <code>service:&lt;name&gt;</code>.</p>"#);
        }
        o.push_str("</section>");
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_s_services_render_with_links_health_and_where_they_run() {
        let snap = crate::demo::Demo::new().snapshot();
        let all = snap.services();
        let o = page(&snap, &all, &Ctx { can_manage: true, csrf: "tok" });
        assert!(o.contains(r#"href="https://pages.fleet.example.com""#), "{o}");
        assert!(o.contains("on agent-box (pages)"), "{o}");
        assert!(o.contains("Fleetlayer sign-in") && o.contains(">Up<"));
        assert!(o.contains("handbook") && o.contains("/_/links/handbook/remove"));
        // Grafana only signs in with Fleetlayer: it's elsewhere, not run by the fleet.
        let elsewhere = o.find("Elsewhere").unwrap();
        assert!(o.find("grafana").unwrap() > elsewhere && o.find("pages.fleet").unwrap() < elsewhere, "{o}");
        assert!(o.contains(r#"action="/_/links""#));
        assert!(!o.contains('\u{2014}'));
        let o = page(&snap, &all, &Ctx { can_manage: false, csrf: "tok" });
        assert!(!o.contains("/_/links"), "no forms without the right to manage links");
    }

    #[test]
    fn nothing_to_show_says_what_fills_it() {
        let o = page(&Snapshot::default(), &[], &Ctx { can_manage: false, csrf: "t" });
        assert!(o.contains("No services yet"));
    }
}
