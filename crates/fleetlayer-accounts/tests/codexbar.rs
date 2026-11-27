//! The list against CodexBar's claude-swap parser (`ClaudeSwapAccountList.swift`, ported below as `parse`), and the
//! port against a fixture from CodexBar's `ClaudeSwapListParserTests.swift` (MIT, see NOTICE).

use fleetlayer_accounts::{CLAUDE, list};
use serde_json::{Value, json};

const STATUSES: &[&str] = &[
    "ok",
    "token_expired",
    "relogin_required",
    "api_key",
    "keychain_unavailable",
    "no_credentials",
    "foreign_credential",
    "unavailable",
];

fn time(v: &Value) -> Result<(), String> {
    let s = v.as_str().ok_or("not a string")?;
    // ISO8601DateFormatter with .withInternetDateTime: an RFC 3339 time with a zone.
    s.parse::<jiff::Timestamp>().map(|_| ()).map_err(|e| format!("{s}: {e}"))
}

fn window(w: Option<&Value>, what: &str) -> Result<(), String> {
    let Some(w) = w else { return Ok(()) };
    let o = w.as_object().ok_or(format!("{what} is not an object"))?;
    o.get("pct").and_then(Value::as_f64).filter(|p| p.is_finite()).ok_or(format!("{what}: pct"))?;
    if let Some(r) = o.get("resetsAt") {
        time(r).map_err(|e| format!("{what}: resetsAt {e}"))?;
    }
    Ok(())
}

/// What CodexBar's parser refuses, as an error; the rows' numbers when it accepts.
fn parse(v: &Value) -> Result<Vec<i64>, String> {
    let o = v.as_object().ok_or("not a JSON object")?;
    if o.get("schemaVersion").and_then(Value::as_i64) != Some(1) {
        return Err("schemaVersion".into());
    }
    if o.contains_key("error") {
        return Err("reported an error".into());
    }
    let accounts = o.get("accounts").and_then(Value::as_array).ok_or("missing accounts array")?;
    let active = o.get("activeAccountNumber").ok_or("missing activeAccountNumber")?;
    let active =
        if active.is_null() { None } else { Some(active.as_i64().filter(|n| *n > 0).ok_or("activeAccountNumber")?) };
    if !o.get("supportsAccountSwitching").unwrap_or(&json!(true)).is_boolean() {
        return Err("supportsAccountSwitching is not a boolean".into());
    }
    let mut numbers = vec![];
    let mut actives = vec![];
    for r in accounts {
        let r = r.as_object().ok_or("row not an object")?;
        let n = r.get("number").and_then(Value::as_i64).filter(|n| *n > 0).ok_or("number")?;
        if numbers.contains(&n) {
            return Err(format!("duplicate account slot {n}"));
        }
        numbers.push(n);
        if r.get("active").and_then(Value::as_bool).ok_or("active")? {
            actives.push(n);
        }
        let s = r.get("usageStatus").and_then(Value::as_str).ok_or("usageStatus")?;
        assert!(STATUSES.contains(&s), "{s} is a status claude-swap emits");
        let u = r.get("usage");
        window(u.and_then(|u| u.get("fiveHour")), "fiveHour")?;
        window(u.and_then(|u| u.get("sevenDay")), "sevenDay")?;
        if let Some(t) = r.get("usageFetchedAt") {
            time(t)?;
        }
    }
    if actives != active.into_iter().collect::<Vec<_>>() {
        return Err("active account fields disagree".into());
    }
    Ok(numbers)
}

#[test]
fn the_port_accepts_codexbars_fixture() {
    let v: Value = serde_json::from_str(include_str!("fixtures/codexbar-schema-v1.json")).unwrap();
    assert_eq!(parse(&v).unwrap(), vec![1, 2]);
}

fn at(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).unwrap().to_string()
}

fn readings() -> Vec<Value> {
    let now = 1_795_000_000_000_i64;
    let acct = |n: u64, uuid: &str, extra: Value| {
        let mut d = json!({"email": format!("a{n}@example.com"), "daysLeft": 20, "warn": false, "number": n,
            "signedIn": true, "signInNeeded": null});
        for (k, v) in extra.as_object().unwrap() {
            d[k] = v.clone();
        }
        json!({"kind": "claude.signin_age", "key": uuid, "node": "hub", "at": at(now), "data": d})
    };
    vec![
        acct(2, "u2", json!({})),
        acct(1, "u1", json!({})),
        acct(3, "u3", json!({"signInNeeded": "the usage endpoint refused the sign-in"})),
        acct(4, "u4", json!({})),
        acct(5, "u5", json!({})),
        // Account 1: the hub's poll, with microseconds and an offset in its reset times, and the Fable week.
        json!({"kind": "claude.usage", "key": "u1", "node": "hub", "at": at(now), "data": {
            "fiveHour": {"pct": 41.0, "resetsAt": "2026-11-27T05:00:00.123456+00:00"},
            "sevenDay": {"pct": 63.5, "resetsAt": "2026-12-01T10:00:00+00:00"},
            "models": {"Fable": {"pct": 12.0, "resetsAt": "2026-12-01T10:00:00+00:00"}},
            "at": now - 60_000, "source": "poll"}}),
        // An older reading from a session elsewhere loses to the hub's.
        json!({"kind": "claude.usage", "key": "u1", "node": "laptop", "at": at(now), "data": {
            "sevenDay": {"pct": 10.0}, "at": now - 3_600_000, "source": "session"}}),
        // Account 2: no 5-hour window (an Ultra plan), and a bad reset time, which is left out.
        json!({"kind": "claude.usage", "key": "u2", "node": "laptop", "at": at(now), "data": {
            "sevenDay": {"pct": 5.0, "resetsAt": "next week"}, "at": now, "source": "session"}}),
        // Account 4's token fails here; account 5's fails only on another machine.
        json!({"kind": "claude.usage", "key": "u4", "node": "hub", "at": at(now), "data": {"sevenDay": {"pct": 1.0}, "at": now}}),
        json!({"kind": "claude.usage", "key": "u5", "node": "hub", "at": at(now), "data": {"sevenDay": {"pct": 1.0}, "at": now}}),
        json!({"kind": "claude.token_health", "key": "u4", "node": "mac", "at": at(now), "data": {"ok": false, "email": "a4@example.com"}}),
        json!({"kind": "claude.token_health", "key": "u5", "node": "other", "at": at(now), "data": {"ok": false, "email": "a5@example.com"}}),
    ]
}

#[test]
fn every_account_shows_with_its_usage() {
    let v = list(&CLAUDE, &readings(), "mac", jiff::Timestamp::now()).unwrap();
    assert_eq!(parse(&v).unwrap(), vec![1, 2, 3, 4, 5], "{v:#}");
    assert_eq!(v["supportsAccountSwitching"], false);
    assert!(v["activeAccountNumber"].is_null());
    let a = &v["accounts"];
    assert_eq!(a[0]["usageStatus"], "ok");
    assert_eq!(a[0]["email"], "a1@example.com");
    assert_eq!(a[0]["usage"]["fiveHour"], json!({"pct": 41.0, "resetsAt": "2026-11-27T05:00:00Z"}));
    assert_eq!(a[0]["usage"]["sevenDay"]["pct"], 63.5);
    assert_eq!(a[0]["usage"]["scoped"], json!([{"name": "Fable", "pct": 12.0, "resetsAt": "2026-12-01T10:00:00Z"}]));
    assert_eq!(a[0]["usageFetchedAt"], "2026-11-18T11:05:40Z", "when the hub measured it, not when it was sent");
    assert!(a[1]["usage"].get("fiveHour").is_none(), "no 5-hour window");
    assert_eq!(a[1]["usage"]["sevenDay"], json!({"pct": 5.0}), "a bad reset time is left out");
    assert_eq!(a[2]["usageStatus"], "relogin_required");
    assert_eq!(a[3]["usageStatus"], "token_expired");
    assert_eq!(a[4]["usageStatus"], "ok", "another machine's broken token isn't this one's");
    for r in a.as_array().unwrap() {
        assert_eq!(r["active"], false);
        assert_eq!(r["organizationName"], "");
    }
}

#[test]
fn an_account_without_usage_is_unavailable() {
    let r = vec![json!({"kind": "claude.signin_age", "key": "u1", "node": "hub", "at": at(0),
        "data": {"email": "a@example.com", "number": 1, "signedIn": true}})];
    let v = list(&CLAUDE, &r, "mac", jiff::Timestamp::now()).unwrap();
    assert_eq!(v["accounts"][0]["usageStatus"], "unavailable");
    assert!(v["accounts"][0].get("usage").is_none());
    parse(&v).unwrap();
}

#[test]
fn failures_are_claude_swap_errors() {
    let e = list(&CLAUDE, &[], "mac", jiff::Timestamp::now()).unwrap_err();
    assert_eq!(e.kind, "NoAccounts");
    assert_eq!(e.json()["schemaVersion"], 1);
    assert!(e.json()["error"]["message"].is_string());
    let old = vec![json!({"kind": "claude.signin_age", "key": "u1", "node": "hub", "at": at(0),
        "data": {"email": "a@example.com", "daysLeft": 3, "warn": false}})];
    assert_eq!(list(&CLAUDE, &old, "mac", jiff::Timestamp::now()).unwrap_err().kind, "HubTooOld");
}
