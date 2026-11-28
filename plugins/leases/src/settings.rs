//! The leases settings file (`~/.config/fleetlayer/leases.yaml` by default): the kinds of lease the authority hands
//! out.
//!
//! ```yaml
//! kinds:
//!   build-server:             # counted slots: at most 2 held at once, the rest wait in turn
//!     slots: 2
//!     ttl: 2h                 # a lease's limit unless --ttl says less (default 2h)
//!     max_ttl: 12h            # the most --ttl may ask for (default 24h)
//!     idle: 30m               # a session holder idle this long loses it (default 30m)
//!   terraform:                # a lock per named thing: one holder per component, the rest wait in turn
//!     slots: 1
//!     per: component          # slots are counted per value of this payload field
//!     payload:
//!       required: [component]
//!       fields:
//!         component: '[a-z0-9_-]{1,40}(/[a-z0-9_-]{1,40})?'
//!   gmail-filter:             # no slots: any number, each approved once per payload
//!     approval: true
//!     ttl: 24h
//!     max_per_holder: 5
//!     events: leases          # start and end events go to this channel
//!     payload:
//!       required: [account, query]
//!       fields:
//!         account: '^[a-z0-9_-]{1,40}$'
//!         query: '.*from:\S+.*'
//! ```
//!
//! A field's pattern must match its whole value.

use std::collections::BTreeMap;

use jiff::SignedDuration;
use regex::Regex;
use serde::Deserialize;

pub const DEFAULT_TTL: SignedDuration = SignedDuration::from_hours(2);
pub const DEFAULT_MAX_TTL: SignedDuration = SignedDuration::from_hours(24);
pub const DEFAULT_IDLE: SignedDuration = SignedDuration::from_mins(30);
/// Small enough that the person always sees a payload whole, with its reason, when asked to approve it.
const MAX_PAYLOAD_BYTES: usize = 600;

/// Characters a person can't see: controls, and the zero-width and direction marks that hide or reorder text.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{00ad}' | '\u{034f}' | '\u{061c}' | '\u{115f}' | '\u{1160}' | '\u{17b4}' | '\u{17b5}'
            | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}'
            | '\u{3164}' | '\u{fe00}'..='\u{fe0f}' | '\u{feff}' | '\u{ffa0}' | '\u{e0000}'..='\u{e007f}')
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    kinds: BTreeMap<String, RawKind>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKind {
    slots: Option<u32>,
    per: Option<String>,
    ttl: Option<String>,
    max_ttl: Option<String>,
    idle: Option<String>,
    #[serde(default)]
    approval: bool,
    max_per_holder: Option<u32>,
    stuck_after: Option<String>,
    events: Option<String>,
    payload: Option<RawPayload>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPayload {
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    fields: BTreeMap<String, String>,
    #[serde(default)]
    others: bool,
}

/// What a lease's payload must look like: a JSON object of short strings.
#[derive(Debug, Clone, Default)]
pub struct PayloadRule {
    pub required: Vec<String>,
    /// Each named field's value must match its pattern (anywhere, unless the pattern anchors itself).
    pub fields: BTreeMap<String, Regex>,
    /// Fields not named in `fields` are allowed.
    pub others: bool,
}

#[derive(Debug, Clone)]
pub struct Kind {
    pub name: String,
    pub description: Option<String>,
    /// At most this many held at once; None: no limit.
    pub slots: Option<u32>,
    /// Slots are counted per value of this payload field: a lock per named thing (one Terraform component, one
    /// database). Leases with different values never wait for each other. None: one count for the whole kind.
    pub per: Option<String>,
    pub ttl: SignedDuration,
    pub max_ttl: SignedDuration,
    pub idle: SignedDuration,
    /// The access policy decides each new payload (`write` on `service:leases/<kind>/<key>`), asking the person when
    /// it says ask.
    pub approval: bool,
    pub max_per_holder: Option<u32>,
    /// Someone waiting this long means the queue is stuck (default: the kind's ttl).
    pub stuck_after: SignedDuration,
    /// The channel that gets `lease.started` and `lease.ended` events; None: no events.
    pub events: Option<String>,
    /// None: the kind takes no payload.
    pub payload: Option<PayloadRule>,
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub kinds: BTreeMap<String, Kind>,
}

/// A kind's name, a payload field's name: lower case, digits, `-`, `_` and `.`, so it's safe in an access target.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
}

/// `30m`, `2h`, `1h30m`, `1d`: at least a minute.
pub fn parse_duration(s: &str) -> Result<SignedDuration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("an empty duration".into());
    }
    let mut total: i64 = 0;
    let mut digits = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return Err(format!("'{s}' isn't a duration: use s, m, h, d or w, as in 30m, 2h or 1h30m")),
        };
        let n: i64 = digits.parse().map_err(|_| format!("'{s}' isn't a duration: a number goes before each unit"))?;
        digits.clear();
        total = n.checked_mul(unit).and_then(|v| total.checked_add(v)).ok_or_else(|| format!("'{s}' is too long"))?;
    }
    if !digits.is_empty() {
        return Err(format!("'{s}' needs a unit after its last number (s, m, h, d or w)"));
    }
    if total < 60 {
        return Err(format!("'{s}' is shorter than a minute"));
    }
    Ok(SignedDuration::from_secs(total))
}

/// Read a settings file's text, reporting every problem at once, one per line.
pub fn parse(text: &str) -> Result<Settings, String> {
    if text.trim().is_empty() {
        return Ok(Settings::default());
    }
    let file: File = yaml_serde::from_str(text).map_err(|e| e.to_string())?;
    let mut problems = vec![];
    let mut kinds = BTreeMap::new();
    for (name, raw) in file.kinds {
        match kind(&name, raw) {
            Ok(k) => {
                kinds.insert(name, k);
            }
            Err(e) => problems.push(format!("kind '{name}': {e}")),
        }
    }
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    Ok(Settings { kinds })
}

fn kind(name: &str, raw: RawKind) -> Result<Kind, String> {
    if !valid_name(name) {
        return Err("a kind's name is lower-case letters, digits, '-', '_' and '.', at most 64".into());
    }
    let dur = |v: &Option<String>, d: SignedDuration, what: &str| match v {
        Some(s) => parse_duration(s).map_err(|e| format!("{what}: {e}")),
        None => Ok(d),
    };
    let ttl = dur(&raw.ttl, DEFAULT_TTL, "ttl")?;
    let max_ttl = dur(&raw.max_ttl, DEFAULT_MAX_TTL.max(ttl), "max_ttl")?;
    if ttl > max_ttl {
        return Err("ttl is longer than max_ttl".into());
    }
    let idle = dur(&raw.idle, DEFAULT_IDLE, "idle")?;
    let stuck_after = dur(&raw.stuck_after, ttl, "stuck_after")?;
    if raw.slots == Some(0) {
        return Err("slots: at least 1 (leave it out for no limit)".into());
    }
    if raw.max_per_holder == Some(0) {
        return Err("max_per_holder: at least 1".into());
    }
    if let Some(ch) = &raw.events
        && (ch.is_empty()
            || ch.len() > 64
            || !ch.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(format!("events: '{ch}' isn't a channel name"));
    }
    let payload = match raw.payload {
        None => None,
        Some(p) => {
            let mut fields = BTreeMap::new();
            for (f, pattern) in p.fields {
                if !valid_name(&f) {
                    return Err(format!("payload field '{f}': lower-case letters, digits, '-', '_' and '.'"));
                }
                // The whole value must match, so a pattern can't be met by a fragment of a longer value.
                let re = Regex::new(&format!("^(?:{pattern})$")).map_err(|e| format!("payload field '{f}': {e}"))?;
                fields.insert(f, re);
            }
            for r in &p.required {
                if !fields.contains_key(r) && !p.others {
                    return Err(format!("payload: '{r}' is required but isn't one of its fields"));
                }
            }
            Some(PayloadRule { required: p.required, fields, others: p.others })
        }
    };
    if let Some(per) = &raw.per {
        if raw.slots.is_none() {
            return Err(format!("per: '{per}' counts slots, so it needs slots"));
        }
        if !payload.as_ref().is_some_and(|p| p.required.contains(per)) {
            return Err(format!("per: '{per}' must be one of the payload's required fields"));
        }
        // A pattern keeps what names one thing tight: without one, any text would be a lock of its own
        if !payload.as_ref().is_some_and(|p| p.fields.contains_key(per)) {
            return Err(format!("per: '{per}' needs a pattern under payload.fields"));
        }
    }
    if raw.approval && payload.is_none() {
        return Err("approval needs a payload: the person approves each payload once".into());
    }
    Ok(Kind {
        name: name.to_string(),
        description: raw.description,
        slots: raw.slots,
        per: raw.per,
        ttl,
        max_ttl,
        idle,
        approval: raw.approval,
        max_per_holder: raw.max_per_holder,
        stuck_after,
        events: raw.events,
        payload,
    })
}

impl Kind {
    /// What a lease's slots are counted against: the value of its `per` field, or None for the whole kind.
    pub fn group_of(&self, payload: Option<&serde_json::Value>) -> Option<String> {
        let per = self.per.as_ref()?;
        payload.and_then(|p| p.get(per)).and_then(|v| v.as_str()).map(normalise)
    }

    /// Check a payload against the kind's rule; the canonical payload (keys sorted) on success.
    pub fn check_payload(&self, payload: Option<&serde_json::Value>) -> Result<Option<serde_json::Value>, String> {
        let Some(rule) = &self.payload else {
            return match payload {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(_) => Err(format!("{} takes no payload", self.name)),
            };
        };
        let Some(serde_json::Value::Object(map)) = payload else {
            return Err(format!(
                "{} needs a payload: a JSON object with {}",
                self.name,
                if rule.required.is_empty() { "its fields".to_string() } else { rule.required.join(", ") }
            ));
        };
        let mut out = serde_json::Map::new();
        // Keys in sorted order, so the same payload always has the same key, whatever order it was written in.
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for k in keys {
            let v = &map[k];
            let Some(s) = v.as_str() else {
                return Err(format!("payload field '{k}' must be a string"));
            };
            if let Some(c) = s.chars().find(|c| hidden(*c)) {
                return Err(format!(
                    "payload field '{k}' holds a control or invisible character (U+{:04X}): the person must see all of it",
                    c as u32
                ));
            }
            // The field a kind counts `per` names one thing in one spelling: `Racks//US/` is `racks/us`
            let normal_value;
            let (s, v) = if self.per.as_deref() == Some(k.as_str()) {
                normal_value = serde_json::Value::String(normalise(s));
                (normal_value.as_str().unwrap_or_default(), &normal_value)
            } else {
                (s, v)
            };
            match rule.fields.get(k) {
                Some(re) if !re.is_match(s) => {
                    return Err(format!("payload field '{k}' doesn't match {}", re.as_str()));
                }
                None if !rule.others => {
                    let known: Vec<&str> = rule.fields.keys().map(String::as_str).collect();
                    return Err(format!("payload field '{k}' isn't one of {}", known.join(", ")));
                }
                _ => {}
            }
            out.insert(k.clone(), v.clone());
        }
        for r in &rule.required {
            if !out.contains_key(r) {
                return Err(format!("the payload needs '{r}'"));
            }
        }
        let v = serde_json::Value::Object(out);
        if v.to_string().len() > MAX_PAYLOAD_BYTES {
            return Err(format!("the payload is longer than {MAX_PAYLOAD_BYTES} bytes"));
        }
        Ok(Some(v))
    }
}

/// One spelling for the thing a `per` field names, so the same thing is always the same lock: trimmed, lower case, and
/// as a path without empty or `.` parts (`./Racks//US/` is `racks/us`).
pub fn normalise(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// A payload's key: the first 16 hex digits of the SHA-256 of its canonical JSON. Leases with the same key claim the
/// same thing (one Gmail filter), and an approval holds for a key.
pub fn payload_key(payload: &serde_json::Value) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(payload.to_string().as_bytes());
    hex::encode(digest)[..16].to_string()
}
