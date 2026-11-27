//! Matching names people and agents type: for "did you mean …?" suggestions when a session or node isn't found.

/// Whether `candidate` is a likely meaning of `typed`, in any case: one contains the other, or they're a few edits
/// apart (a typo: "cutovr" for "cutover", "triag" for "triage").
pub fn similar(typed: &str, candidate: &str) -> bool {
    let (t, c) = (typed.to_lowercase(), candidate.to_lowercase());
    if t.is_empty() || c.is_empty() {
        return false;
    }
    if c.contains(&t) || t.contains(&c) {
        return true;
    }
    // Compare with each word-run of the candidate as well as the whole, so "cutovr" meets "quietdeck cutover".
    let limit = |s: &str| (s.chars().count() / 4).clamp(1, 3);
    let words = std::iter::once(c.as_str()).chain(c.split([' ', '-', '_']));
    words.filter(|w| !w.is_empty()).any(|w| distance(&t, w) <= limit(&t).max(limit(w)))
}

/// Levenshtein distance between two strings, by characters.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typos_and_parts_match_and_strangers_dont() {
        assert!(similar("cutovr", "quietdeck cutover"), "a typo in one word");
        assert!(similar("quietdeck cutovr", "quietdeck cutover"), "a typo in the whole");
        assert!(similar("triage", "quietdeck triage"), "contained");
        assert!(similar("TRIAG", "triage"), "any case");
        assert!(similar("agents-triag", "agents-triage"));
        assert!(!similar("billing", "quietdeck cutover"));
        assert!(!similar("ci", "sso"), "short names need a near-exact match");
        assert!(!similar("", "x"));
    }
}
