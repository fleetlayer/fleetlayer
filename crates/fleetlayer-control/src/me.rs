//! `fleetlayer me` (`link/me`): what one session may use, decided item by item with [`crate::authz::dry`], so asking
//! files no approval request, counts as nobody's first use and writes nothing to the audit log. What the caller may
//! not use is left out, so a restricted agent doesn't learn the fleet's layout or its rule names.
//!
//! What each node shares comes from `link/tools`, kept in memory ([`crate::link::Links`]): a cache every node rebuilds
//! when it links, never state to back up.

use fleetlayer_protocol::link::{Me, MeTarget, MeTool};
use fleetlayer_protocol::rpc::RpcError;

use crate::App;
use crate::authz::Dry;

/// The first version whose nodes report what they share.
const REPORTS_TOOLS: (u64, u64, u64) = (0, 3, 10);

pub async fn answer(app: &App, tenant: &str, node: &str, caller: &str) -> Result<Me, RpcError> {
    let who = format!("{caller}@{node}");
    let me = format!("-@{node}");
    let mut out = Me { address: who.clone(), ..Default::default() };
    out.untrusted = crate::authz::untrusted(app, tenant, &who).await;
    let mut justify = std::collections::BTreeSet::new();

    let fleet = crate::api::fleet(app, tenant).await.map_err(RpcError::internal)?;
    let online = app.links.online(tenant).await;
    // Nodes this one may see at all, as the fleet view decides.
    let mut visible: Vec<(String, Option<String>)> = vec![];
    for n in fleet["nodes"].as_array().into_iter().flatten() {
        let name = n["name"].as_str().unwrap_or_default().to_string();
        if name == node || crate::authz::may_read(app, tenant, &me, &format!("-@{name}")).await {
            visible.push((name, n["version"].as_str().map(String::from)));
        }
    }

    // Sessions on other nodes: the ones it may message, and how many it may not.
    for s in fleet["sessions"].as_array().into_iter().flatten() {
        let on = s["node"].as_str().unwrap_or_default();
        if on == node || !s["ended"].is_null() || !visible.iter().any(|(n, _)| n == on) {
            continue;
        }
        let name = s["meta"]["tab"].as_str().or(s["sessionId"].as_str()).unwrap_or_default();
        let target = format!("{name}@{on}");
        if !crate::authz::may_see(app, tenant, &me, &target).await {
            continue;
        }
        match crate::authz::dry(app, tenant, &who, "message", &target).await {
            Dry::Denied => out.message_refused += 1,
            d => {
                if why(&d) {
                    justify.insert("message");
                }
                out.message.push(target);
            }
        }
    }
    out.message.sort();

    // Shared tools: the sharing node's own allow list, then the access policy.
    let tools = app.links.tools.lock().await.clone();
    for (name, _version) in &visible {
        let Some(list) = tools.get(&(tenant.to_string(), name.clone())) else { continue };
        for t in list {
            if !t.allow.iter().any(|p| crate::acl::glob(p, &who)) {
                continue;
            }
            let target = format!("tool:{}@{name}", t.name);
            let d = crate::authz::dry(app, tenant, &who, "use_tool", &target).await;
            let Some(state) = target_of(&d, &format!("{}@{name}", t.name), name, !online.contains(name)) else {
                continue;
            };
            if why(&d) {
                justify.insert("use_tool");
            }
            out.tools.push(MeTool {
                target: state,
                description: t.description.clone(),
                not_offered: t.not_offered.clone(),
                folders: t.folders.clone(),
            });
        }
    }

    // Pages spaces, by the node that hosts each.
    let pages = app.links.pages.lock().await.clone();
    let mut spaces: Vec<(&String, &String)> =
        pages.iter().filter(|((t, _), _)| t == tenant).map(|((_, space), host)| (space, host)).collect();
    spaces.sort();
    for (space, host) in spaces {
        let d = crate::authz::dry(app, tenant, &who, "write", &format!("page:{space}/*")).await;
        if let Some(state) = target_of(&d, space, host, !online.contains(host)) {
            if why(&d) {
                justify.insert("write");
            }
            out.pages.push(state);
        }
    }

    // Nodes that can't say what they share.
    for (name, version) in &visible {
        if tools.contains_key(&(tenant.to_string(), name.clone())) {
            continue;
        }
        match version.as_deref().and_then(parse_version) {
            Some(v) if v < REPORTS_TOOLS => {
                out.unknown.push(format!("{name} (version {}, older than v0.3.10)", version.as_deref().unwrap_or("?")))
            }
            _ if !online.contains(name) => {
                out.unknown.push(format!("{name} (offline since the control plane started)"))
            }
            _ => {}
        }
    }

    for s in crate::db::subscriptions(&app.db, tenant).await.map_err(RpcError::internal)? {
        if s.target == who {
            out.subscriptions.push(match s.subject {
                Some(subj) => format!("{} ({subj})", s.channel),
                None => s.channel,
            });
        }
    }
    out.justify = justify.into_iter().map(String::from).collect();
    Ok(out)
}

fn why(d: &Dry) -> bool {
    matches!(d, Dry::Allowed { why: true, .. } | Dry::Ask { why: true, .. } | Dry::Refused { why: true, .. })
}

/// A target's line, or None when it's refused by a rule (left out).
fn target_of(d: &Dry, name: &str, node: &str, offline: bool) -> Option<MeTarget> {
    let (state, request, until) = match d {
        Dry::Denied => return None,
        Dry::Allowed { until, .. } => ("allowed", None, *until),
        Dry::Ask { pending: None, .. } => ("ask", None, None),
        Dry::Ask { pending: Some(id), .. } => ("pending", Some(id.clone()), None),
        Dry::Refused { request, .. } => ("refused", Some(request.clone()), None),
    };
    Some(MeTarget { name: name.into(), node: node.into(), state: state.into(), request, until, offline })
}

/// `0.3.9`, `0.3.9-rc.3` or `v0.3.10` as numbers; a pre-release counts as its release.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim_start_matches('v');
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers() {
        assert_eq!(parse_version("0.3.9-rc.3"), Some((0, 3, 9)));
        assert_eq!(parse_version("v0.3.10"), Some((0, 3, 10)));
        assert!(parse_version("0.3.9").unwrap() < REPORTS_TOOLS);
        assert!(parse_version("0.3.10-rc.1").unwrap() >= REPORTS_TOOLS);
        assert_eq!(parse_version("dev"), None);
    }

    #[test]
    fn refused_targets_are_left_out_and_the_rest_say_their_state() {
        assert_eq!(target_of(&Dry::Denied, "browser@laptop", "laptop", false), None);
        let t = target_of(&Dry::Ask { why: false, pending: Some("ap_1".into()) }, "b@l", "l", true).unwrap();
        assert_eq!((t.state.as_str(), t.request.as_deref(), t.offline), ("pending", Some("ap_1"), true));
        let t = target_of(&Dry::Allowed { why: true, until: Some(5) }, "b@l", "l", false).unwrap();
        assert_eq!((t.state.as_str(), t.until), ("allowed", Some(5)));
    }

    async fn fleet() -> std::sync::Arc<App> {
        let app = std::sync::Arc::new(App {
            db: crate::db::open_memory().await.unwrap(),
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        });
        for n in ["laptop", "server"] {
            crate::db::enrol(&app.db, "default", n).await.unwrap();
            crate::people::agent_admit(&app.db, "default", &format!("*@{n}"), "admin").await.unwrap();
        }
        let tool = |name: &str, allow: &[&str]| fleetlayer_protocol::link::ToolInfo {
            name: name.into(),
            allow: allow.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        };
        app.links.tools.lock().await.insert(
            ("default".into(), "laptop".into()),
            vec![tool("browser", &["*@server"]), tool("downloads", &["*@server"]), tool("bank", &["infra@server"])],
        );
        app
    }

    async fn counts(app: &App) -> (i64, i64) {
        let approvals = sqlx::query_scalar("SELECT COUNT(*) FROM approvals").fetch_one(&app.db).await.unwrap();
        let audit = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log").fetch_one(&app.db).await.unwrap();
        (approvals, audit)
    }

    #[tokio::test]
    async fn it_lists_what_may_be_used_without_asking_or_recording_anything() {
        let app = fleet().await;
        let me = answer(&app, "default", "server", "planning").await.unwrap();
        assert_eq!(me.address, "planning@server");
        let names: Vec<(&str, &str)> =
            me.tools.iter().map(|t| (t.target.name.as_str(), t.target.state.as_str())).collect();
        // The built-in default asks before each first use of a tool; bank's own allow list leaves planning out.
        assert_eq!(names, [("browser@laptop", "ask"), ("downloads@laptop", "ask")]);
        assert!(me.tools.iter().all(|t| t.target.offline), "laptop isn't linked");
        assert!(!me.untrusted);
        assert_eq!(counts(&app).await, (0, 0), "asking files no approval request and writes no audit entry");
    }

    #[tokio::test]
    async fn refused_tools_are_left_out_and_approvals_show() {
        let app = fleet().await;
        let policy = "version: 2\ndefault: allow\nuntrusted: [\"planning@server\"]\ndeny:\n  - { who: \"*\", can: use_tool, to: [\"tool:downloads@laptop\"] }\n";
        crate::authz::store(&app.db, "default", policy).await.unwrap();
        let far = crate::db::now_ms() + 86_400_000;
        sqlx::query(
            "INSERT INTO approvals (tenant, id, subject, verb, target, status, hold_ms, created_at, expires_at)
             VALUES ('default', 'ap_1', 'planning@server', 'use_tool', 'tool:browser@laptop', 'approved', 1, 0, ?)",
        )
        .bind(far)
        .execute(&app.db)
        .await
        .unwrap();
        let me = answer(&app, "default", "server", "planning").await.unwrap();
        assert_eq!(me.tools.len(), 1, "downloads is denied, so it isn't named at all");
        assert_eq!((me.tools[0].target.state.as_str(), me.tools[0].target.until), ("allowed", Some(far)));
        assert!(me.untrusted);
        let expires: i64 =
            sqlx::query_scalar("SELECT expires_at FROM approvals WHERE id = 'ap_1'").fetch_one(&app.db).await.unwrap();
        assert_eq!(expires, far, "looking doesn't renew an approval");
    }

    #[tokio::test]
    async fn an_old_node_is_named_as_unknown() {
        let app = fleet().await;
        crate::db::node_seen(&app.db, "default", "server", "m", "u", "0.3.9").await.unwrap();
        app.links.tools.lock().await.clear();
        let me = answer(&app, "default", "laptop", "planning").await.unwrap();
        assert!(me.unknown.iter().any(|u| u.starts_with("server (version 0.3.9")), "{:?}", me.unknown);
    }
}
