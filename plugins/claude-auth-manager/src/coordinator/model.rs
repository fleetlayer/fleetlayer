//! The coordinator's types and the helpers it shares with the rotation plugin,
//! ported from claude-account-rotator's `types/index.d.ts` and `hooks/model.ts`.
//! JSON shapes match the TypeScript exactly: the plugin and the menu bar app read
//! them, and `state.json` is the same file the TypeScript coordinator writes.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Leave a login once its 5-hour window is this full (percent). The node uses it
/// only to tell whether a limit hit is already shown by the usage.
pub const LEAVE_FIVE_HOUR: f64 = 90.0;
/// Leave a login once its weekly window is this full (percent).
pub const LEAVE_SEVEN_DAY: f64 = 98.0;

/// Epoch milliseconds, read from any JSON number (a client may send a float).
pub type Ms = i64;

fn de_ms<'de, D: Deserializer<'de>>(d: D) -> Result<Ms, D::Error> {
    let n = f64::deserialize(d)?;
    Ok(n as i64)
}

fn de_opt_ms<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Ms>, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.map(|n| n as i64))
}

/// One usage window of a login: percent used, and when it resets (ISO 8601).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

/// A login's usage as last read. `models` holds a model family's own weeks (`Fable`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<BTreeMap<String, UsageWindow>>,
    #[serde(deserialize_with = "de_ms")]
    pub at: Ms,
}

/// Whether a login can serve requests, or why not.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Health {
    #[serde(rename_all = "camelCase")]
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "de_opt_ms")]
        refresh_expires_at: Option<Ms>,
    },
    LoginNeeded {
        why: String,
    },
    LoggingIn {
        #[serde(deserialize_with = "de_ms")]
        since: Ms,
    },
}

impl Health {
    pub fn kind(&self) -> &'static str {
        match self {
            Health::Ok { .. } => "ok",
            Health::LoginNeeded { .. } => "login-needed",
            Health::LoggingIn { .. } => "logging-in",
        }
    }
    pub fn login_needed(why: impl Into<String>) -> Health {
        Health::LoginNeeded { why: why.into() }
    }
}

/// One Claude login. `slot` is the credentials directory: '' is the normal login.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub slot: String,
    /// The profile endpoint's `account.uuid`: with `org_id`, the subscription's ID on every machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

/// Who a sign-in belongs to: email, organization, plan and tier, each as far as known.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

impl Account {
    pub fn identity(&self) -> Identity {
        Identity {
            account_uuid: self.account_uuid.clone(),
            email: self.email.clone(),
            org_id: self.org_id.clone(),
            org_name: self.org_name.clone(),
            plan: self.plan.clone(),
            tier: self.tier.clone(),
        }
    }

    /// `{ ...account, ...who }`: each field `who` has replaces the account's.
    pub fn with(&self, who: &Identity) -> Account {
        let pick = |new: &Option<String>, old: &Option<String>| new.clone().or_else(|| old.clone());
        Account {
            id: self.id.clone(),
            slot: self.slot.clone(),
            account_uuid: pick(&who.account_uuid, &self.account_uuid),
            email: pick(&who.email, &self.email),
            org_id: pick(&who.org_id, &self.org_id),
            org_name: pick(&who.org_name, &self.org_name),
            plan: pick(&who.plan, &self.plan),
            tier: pick(&who.tier, &self.tier),
        }
    }
}

/// Who a login's sign-in belongs to, as the profile endpoint answered for the token `token` digests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Holder {
    pub token: String,
    pub who: Identity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Glyphs {
    Bars,
    Pie,
    Braille,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Highlight {
    Lines,
    Tile,
    Dim,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fill {
    Left,
    Used,
}

/// The footer's look: the plugin's setting, kept here so every session sees a change at once.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FooterStyle {
    pub session: Glyphs,
    pub week: Glyphs,
    pub session_percent: bool,
    pub week_percent: bool,
    pub active: Highlight,
    pub fill: Fill,
}

pub const DEFAULT_FOOTER: FooterStyle = FooterStyle {
    session: Glyphs::Braille,
    week: Glyphs::Bars,
    session_percent: true,
    week_percent: true,
    active: Highlight::Lines,
    fill: Fill::Left,
};

pub const ROTATE_TOS: [&str; 4] = ["reset", "room", "order", "off"];
pub const DEFAULT_ROTATE_TO: &str = "reset";

/// Which login the rotation moves to, as stored; unknown is the default's.
pub fn rotate_to_from(value: Option<&Value>) -> String {
    match value.and_then(Value::as_str) {
        Some(s) if ROTATE_TOS.contains(&s) => s.to_string(),
        _ => DEFAULT_ROTATE_TO.to_string(),
    }
}

fn one_of<T: for<'de> Deserialize<'de>>(v: Option<&Value>) -> Option<T> {
    v.filter(|v| v.is_string()).and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// The footer's look as stored; any part missing or unknown is the default's.
pub fn footer_style_from(value: Option<&Value>) -> FooterStyle {
    let get = |k: &str| value.and_then(|v| v.as_object()).and_then(|o| o.get(k));
    let d = DEFAULT_FOOTER;
    FooterStyle {
        session: one_of(get("session")).unwrap_or(d.session),
        week: one_of(get("week")).unwrap_or(d.week),
        session_percent: get("sessionPercent").and_then(Value::as_bool).unwrap_or(d.session_percent),
        week_percent: get("weekPercent").and_then(Value::as_bool).unwrap_or(d.week_percent),
        active: one_of(get("active")).unwrap_or(d.active),
        fill: one_of(get("fill")).unwrap_or(d.fill),
    }
}

/// An ISO 8601 time as epoch ms, as JavaScript's `Date.parse` reads the ones Anthropic sends.
pub fn parse_iso(s: &str) -> Option<Ms> {
    if let Ok(ts) = s.parse::<jiff::Timestamp>() {
        return Some(ts.as_millisecond());
    }
    // A date-time with no offset: JavaScript reads it as local time.
    s.parse::<jiff::civil::DateTime>()
        .ok()
        .and_then(|dt| dt.to_zoned_datetime(jiff::tz::TimeZone::system()).ok())
        .map(|z| z.timestamp().as_millisecond())
}

/// Epoch ms as JavaScript's `toISOString` writes it: `2026-11-09T12:00:00.000Z`.
pub fn iso(ms: Ms) -> String {
    match jiff::Timestamp::from_millisecond(ms) {
        Ok(ts) => ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        Err(_) => String::new(),
    }
}

/// A window's percent now: 0 once its reset time has passed.
pub fn window_pct(w: Option<&UsageWindow>, now: Ms) -> Option<f64> {
    let w = w?;
    if let Some(at) = w.resets_at.as_deref().and_then(parse_iso)
        && at <= now
    {
        return Some(0.0);
    }
    Some(w.pct)
}

/// A login's week for one model family, by name.
pub fn model_window<'a>(r: Option<&'a Reading>, scope: Option<&str>) -> Option<&'a UsageWindow> {
    r?.models.as_ref()?.get(scope?)
}

/// Past a leave threshold: the 5-hour window, the week, or (with `scope`) the model's own week.
pub fn is_spent(r: Option<&Reading>, now: Ms, scope: Option<&str>) -> bool {
    window_pct(r.and_then(|r| r.five_hour.as_ref()), now).unwrap_or(0.0) >= LEAVE_FIVE_HOUR
        || window_pct(r.and_then(|r| r.seven_day.as_ref()), now).unwrap_or(0.0) >= LEAVE_SEVEN_DAY
        || window_pct(model_window(r, scope), now).unwrap_or(0.0) >= LEAVE_SEVEN_DAY
}

/// The window that limits a login: its 5-hour window where its plan reports one, else its week.
fn limits_five_hour(r: Option<&Reading>) -> bool {
    r.is_some_and(|r| r.five_hour.is_some())
}

/// A login's reading once a turn hit its usage limit before its usage showed it: the limiting window full.
pub fn spent_reading(r: Option<&Reading>, now: Ms, scope: Option<&str>) -> Reading {
    let mut out = r.cloned().unwrap_or(Reading { five_hour: None, seven_day: None, models: None, at: now });
    out.at = now;
    if let Some(scope) = scope {
        let week = window_pct(r.and_then(|r| r.seven_day.as_ref()), now).unwrap_or(0.0);
        if week < LEAVE_SEVEN_DAY && !limits_five_hour(r) {
            let resets_at =
                model_window(r, Some(scope)).or(r.and_then(|r| r.seven_day.as_ref())).and_then(|w| w.resets_at.clone());
            out.models
                .get_or_insert_with(BTreeMap::new)
                .insert(scope.to_string(), UsageWindow { pct: 100.0, resets_at });
            return out;
        }
    }
    if limits_five_hour(r) {
        let resets_at = r.and_then(|r| r.five_hour.as_ref()).and_then(|w| w.resets_at.clone());
        out.five_hour = Some(UsageWindow { pct: 100.0, resets_at });
    } else {
        let resets_at = r.and_then(|r| r.seven_day.as_ref()).and_then(|w| w.resets_at.clone());
        out.seven_day = Some(UsageWindow { pct: 100.0, resets_at });
    }
    out
}

fn window_from(v: Option<&Value>) -> Option<UsageWindow> {
    let o = v?.as_object()?;
    let pct = o.get("utilization")?.as_f64()?;
    Some(UsageWindow { pct, resets_at: o.get("resets_at").and_then(Value::as_str).map(str::to_string) })
}

/// A model family's own weeks from the usage body's `limits`: each `weekly_scoped` limit scoped to a model.
fn model_windows_from(limits: Option<&Value>) -> Option<BTreeMap<String, UsageWindow>> {
    let mut models = BTreeMap::new();
    for limit in limits?.as_array()? {
        let Some(l) = limit.as_object() else { continue };
        let scope = l.get("scope");
        let model = scope.and_then(|s| s.get("model")).and_then(|m| m.get("display_name")).and_then(Value::as_str);
        let surface = scope.and_then(|s| s.get("surface")).filter(|s| !s.is_null());
        let (Some(model), Some(pct)) = (model, l.get("percent").and_then(Value::as_f64)) else {
            continue;
        };
        if l.get("kind").and_then(Value::as_str) != Some("weekly_scoped") || model.is_empty() || surface.is_some() {
            continue;
        }
        models.insert(
            model.to_string(),
            UsageWindow { pct, resets_at: l.get("resets_at").and_then(Value::as_str).map(str::to_string) },
        );
    }
    (!models.is_empty()).then_some(models)
}

/// A reading from the body of GET /api/oauth/usage.
pub fn reading_from_usage(body: &Value, now: Ms) -> Option<Reading> {
    let o = body.as_object()?;
    let five_hour = window_from(o.get("five_hour"));
    let seven_day = window_from(o.get("seven_day"));
    let models = model_windows_from(o.get("limits"));
    if five_hour.is_none() && seven_day.is_none() && models.is_none() {
        return None;
    }
    Some(Reading { five_hour, seven_day, models, at: now })
}

fn client_window(v: Option<&Value>) -> Option<UsageWindow> {
    let o = v?.as_object()?;
    let pct = o.get("pct")?.as_f64().filter(|p| p.is_finite())?;
    Some(UsageWindow { pct, resets_at: o.get("resetsAt").and_then(Value::as_str).map(str::to_string) })
}

/// A reading a client sent, checked: None when it isn't one.
pub fn reading_from_client(v: &Value) -> Option<Reading> {
    let o = v.as_object()?;
    let at = o.get("at")?.as_f64()? as i64;
    let five_hour = client_window(o.get("fiveHour"));
    let seven_day = client_window(o.get("sevenDay"));
    let models: BTreeMap<String, UsageWindow> = o
        .get("models")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, w)| client_window(Some(w)).map(|w| (k.clone(), w))).collect())
        .unwrap_or_default();
    let models = (!models.is_empty()).then_some(models);
    if five_hour.is_none() && seven_day.is_none() && models.is_none() {
        return None;
    }
    Some(Reading { five_hour, seven_day, models, at })
}

/// Two readings that show the same: within half a point on each window, with the same resets.
pub fn same_reading(a: Option<&Reading>, b: &Reading) -> bool {
    let Some(a) = a else { return false };
    let same = |x: Option<&UsageWindow>, y: Option<&UsageWindow>| match (x, y) {
        (None, None) => true,
        (Some(x), Some(y)) => (x.pct - y.pct).abs() < 0.5 && x.resets_at == y.resets_at,
        _ => false,
    };
    let empty = BTreeMap::new();
    let (am, bm) = (a.models.as_ref().unwrap_or(&empty), b.models.as_ref().unwrap_or(&empty));
    same(a.five_hour.as_ref(), b.five_hour.as_ref())
        && same(a.seven_day.as_ref(), b.seven_day.as_ref())
        && am.keys().chain(bm.keys()).all(|n| same(am.get(n), bm.get(n)))
}

/// The part of a stored login that matters here.
#[derive(Clone, Debug, PartialEq)]
pub struct Credential {
    pub access_token: String,
    pub expires_at: Ms,
    pub refresh_token_expires_at: Option<Ms>,
    pub refresh_token: Option<String>,
    pub plan: Option<String>,
}

/// A stored sign-in's text (Keychain entry or credentials file) read.
pub fn credential_from(text: &str) -> Option<Credential> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    let o = parsed.get("claudeAiOauth")?.as_object()?;
    Some(Credential {
        access_token: o.get("accessToken")?.as_str()?.to_string(),
        expires_at: o.get("expiresAt")?.as_f64()? as i64,
        refresh_token_expires_at: o.get("refreshTokenExpiresAt").and_then(Value::as_f64).map(|n| n as i64),
        refresh_token: o.get("refreshToken").and_then(Value::as_str).map(str::to_string),
        plan: o.get("subscriptionType").and_then(Value::as_str).map(str::to_string),
    })
}

/// A login's health from its stored credential alone.
pub fn health_of(cred: Option<&Credential>, now: Ms) -> Health {
    let Some(cred) = cred else {
        return Health::login_needed("not signed in");
    };
    if let Some(at) = cred.refresh_token_expires_at
        && at <= now
    {
        return Health::login_needed("sign-in expired");
    }
    Health::Ok { refresh_expires_at: cred.refresh_token_expires_at.filter(|&at| at != 0) }
}

/// "2 (two@example.com · Org)", or the number before the profile has been read.
pub fn label(account: Option<&Account>) -> String {
    match account {
        None => "?".into(),
        Some(a) => match &a.email {
            None => a.id.clone(),
            Some(email) => match &a.org_name {
                Some(org) if !org.is_empty() => format!("{} ({email} · {org})", a.id),
                _ => format!("{} ({email})", a.id),
            },
        },
    }
}

pub use crate::claude::{credentials_file, keychain_item, keychain_user, slot_env};

/// A digest of a sign-in's token: tells a new sign-in from the one asked about, without keeping the token.
pub fn fingerprint(token: &str) -> String {
    crate::claude::sha256_hex(token)[..16].to_string()
}

/// The first number from 2 up that no login uses yet.
pub fn free_id(accounts: &[Account]) -> String {
    let mut n = 2;
    while accounts.iter().any(|a| a.id == n.to_string()) {
        n += 1;
    }
    n.to_string()
}

fn str_field(o: &serde_json::Map<String, Value>, k: &str) -> Option<String> {
    o.get(k).and_then(Value::as_str).map(str::to_string)
}

/// Who a slot is signed in as, from `claude auth status --json`; None when signed out.
pub fn identity_from_status(stdout: &str) -> Option<Identity> {
    let v: Value = serde_json::from_str(stdout).ok()?;
    let o = v.as_object()?;
    if o.get("loggedIn") != Some(&Value::Bool(true)) {
        return None;
    }
    Some(Identity {
        account_uuid: None,
        email: Some(o.get("email")?.as_str()?.to_string()),
        org_id: str_field(o, "orgId"),
        org_name: str_field(o, "orgName"),
        plan: str_field(o, "subscriptionType"),
        tier: None,
    })
}

/// Who a token signs in, from the body of GET /api/oauth/profile.
pub fn identity_from_profile(body: Option<&Value>) -> Option<Identity> {
    let body = body?.as_object()?;
    let field = |obj: &str, k: &str| body.get(obj).and_then(Value::as_object).and_then(|o| str_field(o, k));
    let email = field("account", "email").filter(|e| !e.is_empty())?;
    Some(Identity {
        account_uuid: field("account", "uuid").filter(|u| !u.is_empty()),
        email: Some(email),
        org_id: field("organization", "uuid"),
        org_name: field("organization", "name"),
        plan: None,
        tier: field("organization", "rate_limit_tier").filter(|t| !t.is_empty()),
    })
}

/// The same login: one email in one organization.
pub fn same_login(a: &Identity, b: &Identity) -> bool {
    a.email.is_some() && a.email == b.email && a.org_id.as_deref().unwrap_or("") == b.org_id.as_deref().unwrap_or("")
}

/// A listed login is the account `who` signed in as: by its email, and its organization once known.
pub fn is_login_of(a: &Identity, who: &Identity) -> bool {
    a.email.is_some() && a.email == who.email && (a.org_id.is_none() || a.org_id == who.org_id)
}

/// How much weekly usage a login's plan has, in Pros: Ultra 10, Max 5, anything else 1.
pub fn weight_of(tier: Option<&str>) -> u32 {
    let tier = tier.unwrap_or("");
    if tier.contains("max_20x") || tier.contains("ultra") {
        10
    } else if tier.contains("max_5x") {
        5
    } else {
        1
    }
}

/// One usage window across every signed-in login with a reading, weighted by plan size.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CombinedWindow {
    pub pct: f64,
    pub accounts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_reset_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Combined {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<CombinedWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<CombinedWindow>,
}

/// JavaScript's `Math.round(x * 10) / 10`.
fn round_tenth(x: f64) -> f64 {
    (x * 10.0 + 0.5).floor() / 10.0
}

/// Usage across logins, one window at a time: the plan-weighted mean of what each signed-in, read login used.
pub fn combined_usage(
    accounts: &[Account],
    readings: &BTreeMap<String, Reading>,
    health: &BTreeMap<String, Health>,
    now: Ms,
) -> Combined {
    let combine = |pick: &dyn Fn(&Reading) -> Option<&UsageWindow>| -> Option<CombinedWindow> {
        let (mut total, mut weights, mut counted) = (0.0, 0.0, 0u32);
        let mut soonest: Option<Ms> = None;
        for a in accounts {
            if matches!(health.get(&a.id), Some(Health::LoginNeeded { .. })) {
                continue;
            }
            let Some(w) = readings.get(&a.id).and_then(pick) else {
                continue;
            };
            let Some(pct) = window_pct(Some(w), now) else {
                continue;
            };
            let weight = weight_of(a.tier.as_deref()) as f64;
            total += weight * pct;
            weights += weight;
            counted += 1;
            if let Some(reset) = w.resets_at.as_deref().and_then(parse_iso)
                && reset > now
                && soonest.is_none_or(|s| reset < s)
            {
                soonest = Some(reset);
            }
        }
        (counted > 0).then(|| CombinedWindow {
            pct: round_tenth(total / weights),
            accounts: counted,
            next_reset_at: soonest.map(iso),
        })
    };
    Combined { five_hour: combine(&|r| r.five_hour.as_ref()), seven_day: combine(&|r| r.seven_day.as_ref()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keychain_item_matches_claude_code() {
        assert_eq!(keychain_item(""), "Claude Code-credentials");
        // As the TypeScript computes it for the same folder.
        let item = keychain_item("/Users/tester/.claude/accounts/2");
        assert!(item.starts_with("Claude Code-credentials-") && item.len() == "Claude Code-credentials-".len() + 8);
    }

    #[test]
    fn reads_usage_bodies_with_model_weeks() {
        let body = json!({
            "five_hour": null,
            "seven_day": { "utilization": 30, "resets_at": "2026-11-14T03:00:00Z" },
            "limits": [
                { "kind": "weekly_scoped", "percent": 99, "resets_at": "2026-11-14T03:00:00Z", "scope": { "model": { "display_name": "Fable" }, "surface": null } },
                { "kind": "weekly_scoped", "percent": 50, "scope": { "model": { "display_name": "Other" }, "surface": "web" } }
            ]
        });
        let r = reading_from_usage(&body, 5).unwrap();
        assert_eq!(r.five_hour, None);
        assert_eq!(r.seven_day.unwrap().pct, 30.0);
        assert_eq!(r.models.unwrap().keys().collect::<Vec<_>>(), vec!["Fable"]);
    }

    #[test]
    fn a_window_is_empty_once_it_has_reset() {
        let now = parse_iso("2026-11-09T10:00:00Z").unwrap();
        let w = UsageWindow { pct: 95.0, resets_at: Some("2026-11-09T09:00:00Z".into()) };
        assert_eq!(window_pct(Some(&w), now), Some(0.0));
    }

    #[test]
    fn iso_round_trips_as_javascript_writes_it() {
        let ms = parse_iso("2026-11-09T12:00:00Z").unwrap();
        assert_eq!(iso(ms), "2026-11-09T12:00:00.000Z");
    }

    #[test]
    fn combined_weighs_by_plan_and_skips_logins_needing_a_sign_in() {
        let accounts = vec![
            Account { id: "1".into(), tier: Some("default_claude_max_20x".into()), ..Default::default() },
            Account { id: "2".into(), tier: Some("default_claude_max_5x".into()), ..Default::default() },
            Account { id: "3".into(), ..Default::default() },
        ];
        let r = |pct: f64| Reading {
            five_hour: None,
            seven_day: Some(UsageWindow { pct, resets_at: None }),
            models: None,
            at: 0,
        };
        let readings = BTreeMap::from([("1".into(), r(30.0)), ("2".into(), r(60.0)), ("3".into(), r(90.0))]);
        let health = BTreeMap::from([("3".to_string(), Health::login_needed("x"))]);
        let c = combined_usage(&accounts, &readings, &health, 0);
        assert_eq!(c.seven_day.unwrap(), CombinedWindow { pct: 40.0, accounts: 2, next_reset_at: None });
    }

    #[test]
    fn health_json_matches_the_typescript() {
        assert_eq!(
            serde_json::to_value(Health::login_needed("x")).unwrap(),
            json!({ "kind": "login-needed", "why": "x" })
        );
        assert_eq!(
            serde_json::to_value(Health::Ok { refresh_expires_at: Some(5) }).unwrap(),
            json!({ "kind": "ok", "refreshExpiresAt": 5 })
        );
        let h: Health = serde_json::from_value(json!({ "kind": "logging-in", "since": 1.0 })).unwrap();
        assert_eq!(h, Health::LoggingIn { since: 1 });
    }
}
