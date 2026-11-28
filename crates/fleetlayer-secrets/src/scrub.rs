//! Exact-match redaction of delivered secrets. The node knows every value it delivered, so it can find them with no
//! guessing and no false positives on look-alikes: each value, and the forms it takes once something has encoded it,
//! is replaced by `[REDACTED <reference>]`.
//!
//! Forms matched: the value itself; JSON-escaped once and twice (a value inside a JSON string, and inside JSON carried
//! in a JSON string, as transcripts and tool results do); percent-encoded (in a URL); base64, standard and URL-safe,
//! at each of the three byte alignments it can start at inside a longer encoded text; and for long values, a long
//! prefix (a token cut short on screen still names most of it). Values shorter than [`MIN_LEN`] aren't matched: too
//! many ordinary strings would look like them.

use std::borrow::Cow;

use aho_corasick::{AhoCorasick, MatchKind};
use base64::Engine as _;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};

use crate::Secret;

/// The shortest value matched.
pub const MIN_LEN: usize = 8;
/// Values at least this long are matched by their prefix too.
const PREFIX_FROM: usize = 32;
/// The shortest base64 run matched.
const B64_MIN: usize = 16;

/// What URL encoding leaves alone (RFC 3986's unreserved characters).
const URL: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// A form a secret was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Form {
    Plain,
    Json,
    Json2,
    Url,
    Base64,
    Prefix,
}

impl Form {
    pub fn label(self) -> &'static str {
        match self {
            Form::Plain => "plain",
            Form::Json => "JSON-escaped",
            Form::Json2 => "JSON-escaped twice",
            Form::Url => "URL-encoded",
            Form::Base64 => "base64",
            Form::Prefix => "a long prefix",
        }
    }
}

/// One secret found, by its reference: never the value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hit {
    pub reference: String,
    pub form: Form,
}

#[derive(Default)]
pub struct Scrubber {
    matcher: Option<AhoCorasick>,
    /// Pattern index → (secret index, form).
    patterns: Vec<(usize, Form)>,
    references: Vec<String>,
    /// Each secret's value (trimmed), to redact the rest of a prefix match that runs on.
    values: Vec<String>,
    /// References of values too short to match.
    short: Vec<String>,
    /// The longest form it matches, in bytes: how much of a text cut into pieces must carry over to the next piece
    /// so a secret across the cut is still found.
    longest: usize,
}

impl std::fmt::Debug for Scrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scrubber").field("secrets", &self.references.len()).field("short", &self.short).finish()
    }
}

fn json_inner(s: &str) -> String {
    let q = serde_json::to_string(s).unwrap_or_default();
    q[1..q.len() - 1].to_string()
}

/// The base64 characters that depend only on `v`'s bytes, when it starts `k` bytes into the encoded text.
fn base64_cores(v: &[u8]) -> Vec<String> {
    let mut out = vec![];
    for k in 0..3usize {
        let mut buf = vec![0u8; k];
        buf.extend_from_slice(v);
        let n = buf.len();
        let first = (8 * k).div_ceil(6);
        let last = (8 * n) / 6; // exclusive: characters whose six bits are all inside the input
        for engine in [&base64::engine::general_purpose::STANDARD, &base64::engine::general_purpose::URL_SAFE] {
            let enc = engine.encode(&buf);
            if last > first && last - first >= B64_MIN && last <= enc.len() {
                out.push(enc[first..last].to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

impl Scrubber {
    pub fn new(secrets: &[Secret]) -> Scrubber {
        let mut s = Scrubber::default();
        let mut pats: Vec<String> = vec![];
        for sec in secrets {
            let mut values = vec![sec.value.as_str()];
            let trimmed = sec.value.trim();
            if trimmed != sec.value {
                values.push(trimmed); // a token file's value often ends in a newline that isn't part of the token
            }
            if trimmed.chars().count() < MIN_LEN {
                if !s.short.contains(&sec.reference) {
                    s.short.push(sec.reference.clone());
                }
                continue;
            }
            let idx = s.references.len();
            s.references.push(sec.reference.clone());
            s.values.push(trimmed.to_string());
            let mut forms: Vec<(String, Form)> = vec![];
            for v in values {
                if v.chars().count() < MIN_LEN {
                    continue;
                }
                forms.push((v.to_string(), Form::Plain));
                let j = json_inner(v);
                let j2 = json_inner(&j);
                forms.push((j, Form::Json));
                forms.push((j2, Form::Json2));
                forms.push((percent_encoding::utf8_percent_encode(v, URL).to_string(), Form::Url));
                for b in base64_cores(v.as_bytes()) {
                    forms.push((b, Form::Base64));
                }
                let chars: Vec<char> = v.chars().collect();
                if chars.len() >= PREFIX_FROM {
                    let keep = (chars.len() / 2).max(16);
                    forms.push((chars[..keep].iter().collect(), Form::Prefix));
                }
            }
            let mut seen = std::collections::HashSet::new();
            for (p, f) in forms {
                if !p.is_empty() && seen.insert(p.clone()) {
                    s.longest = s.longest.max(p.len());
                    pats.push(p);
                    s.patterns.push((idx, f));
                }
            }
        }
        if !pats.is_empty() {
            s.matcher = AhoCorasick::builder().match_kind(MatchKind::LeftmostLongest).build(&pats).ok();
        }
        s
    }

    pub fn is_empty(&self) -> bool {
        self.matcher.is_none()
    }

    /// How many secrets it matches.
    pub fn len(&self) -> usize {
        self.references.len()
    }

    /// The longest text it matches, in bytes (0 when it matches nothing).
    pub fn longest(&self) -> usize {
        self.longest
    }

    /// References whose values are too short to match.
    pub fn too_short(&self) -> &[String] {
        &self.short
    }

    /// `text` with every secret replaced by `[REDACTED <reference>]`, and what was found (each reference and form once).
    pub fn scrub<'a>(&self, text: &'a str) -> (Cow<'a, str>, Vec<Hit>) {
        let Some(m) = &self.matcher else { return (Cow::Borrowed(text), vec![]) };
        let mut out = String::new();
        let mut last = 0;
        let mut hits = vec![];
        for mat in m.find_iter(text) {
            let (idx, form) = self.patterns[mat.pattern().as_usize()];
            if mat.start() < last {
                continue; // inside a prefix match that ran on
            }
            out.push_str(&text[last..mat.start()]);
            out.push_str("[REDACTED ");
            out.push_str(&self.references[idx]);
            out.push(']');
            last = mat.end();
            if form == Form::Prefix {
                // A value cut short after the prefix: hide whatever more of it there is.
                let rest = &self.values[idx][mat.end() - mat.start()..];
                let more = text[last..]
                    .chars()
                    .zip(rest.chars())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a.len_utf8())
                    .sum::<usize>();
                last += more;
            }
            let h = Hit { reference: self.references[idx].clone(), form };
            if !hits.contains(&h) {
                hits.push(h);
            }
        }
        if hits.is_empty() {
            return (Cow::Borrowed(text), hits);
        }
        out.push_str(&text[last..]);
        (Cow::Owned(out), hits)
    }

    /// Whether `text` holds any secret, without copying it.
    pub fn finds(&self, text: &str) -> bool {
        self.matcher.as_ref().is_some_and(|m| m.is_match(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(r: &str, v: &str) -> Secret {
        Secret { reference: r.into(), value: v.into() }
    }

    const TOKEN: &str = "ghp_9aZk3LmQ7xV1bN8cR2tY6uW4oP0sE5dF";

    #[test]
    fn plain_values_are_redacted_by_reference() {
        let sc = Scrubber::new(&[s("op://github/bot/token", TOKEN)]);
        let text = format!("export GITHUB_TOKEN={TOKEN}; echo ok");
        let (out, hits) = sc.scrub(&text);
        assert_eq!(out, "export GITHUB_TOKEN=[REDACTED op://github/bot/token]; echo ok");
        assert_eq!(hits, vec![Hit { reference: "op://github/bot/token".into(), form: Form::Plain }]);
        let (same, none) = sc.scrub("nothing here");
        assert!(matches!(same, Cow::Borrowed(_)) && none.is_empty());
    }

    #[test]
    fn encoded_forms_are_found() {
        let odd = r#"pa"ss\wo rd/+&=1234"#;
        let sc = Scrubber::new(&[s("op://v/i/f", odd)]);
        let json = serde_json::to_string(&serde_json::json!({"text": odd})).unwrap();
        let (out, hits) = sc.scrub(&json);
        assert!(!out.contains("ss\\\\wo"), "{out}");
        assert_eq!(hits[0].form, Form::Json);
        let twice = serde_json::to_string(&serde_json::json!({"line": json})).unwrap();
        assert_eq!(sc.scrub(&twice).1[0].form, Form::Json2);
        let url = format!("https://x.example/?p={}", percent_encoding::utf8_percent_encode(odd, URL));
        assert_eq!(sc.scrub(&url).1[0].form, Form::Url);
        // Valid JSON stays valid JSON: the marker has no quotes or backslashes.
        let (out, _) = sc.scrub(&twice);
        assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok(), "{out}");
    }

    #[test]
    fn base64_is_found_at_every_alignment() {
        let sc = Scrubber::new(&[s("op://v/i/f", TOKEN)]);
        for pre in ["", "a", "ab", "abc"] {
            for engine in [&base64::engine::general_purpose::STANDARD, &base64::engine::general_purpose::URL_SAFE] {
                let enc = engine.encode(format!("{pre}Authorization: token {TOKEN}\n"));
                let (out, hits) = sc.scrub(&enc);
                assert!(!hits.is_empty(), "missed with prefix {pre:?}: {enc}");
                assert!(out.contains("[REDACTED op://v/i/f]"));
            }
        }
    }

    #[test]
    fn a_long_prefix_is_found_and_the_whole_value_wins_over_it() {
        let sc = Scrubber::new(&[s("op://v/i/f", TOKEN)]);
        let cut = &TOKEN[..20];
        let text = format!("token {cut}… (cut)");
        let (out, hits) = sc.scrub(&text);
        assert_eq!(out, "token [REDACTED op://v/i/f]… (cut)");
        assert_eq!(hits[0].form, Form::Prefix);
        let (out, hits) = sc.scrub(TOKEN);
        assert_eq!(out, "[REDACTED op://v/i/f]");
        assert_eq!(hits[0].form, Form::Plain);
    }

    #[test]
    fn short_values_are_left_out_and_named() {
        let sc = Scrubber::new(&[s("op://v/i/pin", "1234"), s("op://v/i/t", "trailing-newline-token\n")]);
        assert_eq!(sc.too_short(), ["op://v/i/pin"]);
        assert_eq!(sc.len(), 1);
        assert_eq!(sc.scrub("pin 1234, trailing-newline-token.").0, "pin 1234, [REDACTED op://v/i/t].");
        assert!(Scrubber::new(&[]).is_empty());
    }

    #[test]
    fn unicode_values_and_text_are_safe() {
        let sc = Scrubber::new(&[s("op://v/i/f", "pässwörd-ünïcode")]);
        let (out, hits) = sc.scrub("née: pässwörd-ünïcode ✓");
        assert_eq!(out, "née: [REDACTED op://v/i/f] ✓");
        assert_eq!(hits.len(), 1);
        let json = serde_json::to_string("pässwörd-ünïcode").unwrap();
        assert!(sc.finds(&json));
    }
}
