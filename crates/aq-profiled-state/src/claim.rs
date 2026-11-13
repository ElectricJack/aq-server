//! The `Changes:` and `Outcome:` fields of a worker's one-line result conclusion
//! (`Outcome: done; Changes: src/lib.rs; Copied: …; …`, or an agent's own
//! `Changes: …; Checks: …`). Worker text is never evidence for completion: these
//! readings only refuse or explain, and fail open (review R1-I1). aq-worker's
//! `aq_handoff_note::claim` reads the agent's final message with the same rules
//! and the same table of cases.

/// The value of `key:` on one line, tolerant of case, a list bullet or quote
/// marker, and markdown emphasis or code around the key (`**Outcome:** blocked`).
pub fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    const MARKS: [char; 4] = ['*', '_', '`', ' '];
    let line = line.trim_start().trim_start_matches(['-', '>', ' ']);
    let line = line.trim_start_matches(MARKS);
    let head = line.get(..key.len())?;
    if !head.eq_ignore_ascii_case(key) {
        return None;
    }
    let rest = line[key.len()..].trim_start_matches(['*', '_', '`']);
    Some(rest.strip_prefix(':')?.trim_start_matches(MARKS).trim())
}

/// The value of `key` among a conclusion's leading `;`-separated fields.
fn leading<'a>(conclusion: &'a str, key: &str) -> Option<&'a str> {
    conclusion.split(';').take(4).find_map(|f| field(f, key))
}
/// Review I-2, R1-I1: the conclusion claims changes (its `Changes:` value names a
/// path). An older worker writes the copied-back files there.
pub fn claims_changes(conclusion: &str) -> bool {
    leading(conclusion, "changes").is_some_and(names_paths)
}
/// Owner ruling 4a, R1-M2: the conclusion opens with `Outcome: blocked`.
pub fn blocked(conclusion: &str) -> bool {
    leading(conclusion, "outcome").is_some_and(said_blocked)
}
/// An `Outcome:` value whose first word is `blocked`.
pub fn said_blocked(value: &str) -> bool {
    first_word(value).eq_ignore_ascii_case("blocked")
}

fn bare(token: &str) -> &str {
    token
        .trim_matches([
            '*', '_', '`', '\'', '"', '(', ')', '[', ']', '<', '>', '{', '}',
        ])
        .trim_end_matches(['.', ',', ':', ';', '!', '?'])
        .trim_matches(['*', '_', '`', '\'', '"', '(', ')', '[', ']'])
}
fn first_word(value: &str) -> &str {
    value
        .split_whitespace()
        .map(bare)
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}
/// A token that names a file or directory: `dir/name`, `dir/`, or `name.ext`.
fn path_like(token: &str) -> bool {
    let t = bare(token);
    if t.contains('/') {
        let lower = t.to_ascii_lowercase();
        return t
            .split('/')
            .any(|s| s.chars().filter(|c| c.is_alphanumeric()).count() >= 2)
            && !["and/or", "either/or", "yes/no"].contains(&lower.as_str());
    }
    let Some((name, ext)) = t.rsplit_once('.') else {
        return false;
    };
    name.chars().count() >= 2
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || "-_.".contains(c))
        && (1..=10).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}
const NEGATIONS: [&str; 13] = [
    "none",
    "no",
    "nothing",
    "n/a",
    "na",
    "-",
    "nil",
    "null",
    "unchanged",
    "read-only",
    "readonly",
    "zero",
    "0",
];

/// Review R1-I1: a `Changes:` value claims changes only when it names at least one
/// path-like token and does not start with a negation. Anything else is no claim.
pub fn names_paths(value: &str) -> bool {
    let first = first_word(value).to_ascii_lowercase();
    !NEGATIONS.contains(&first.as_str())
        && value
            .split(|c: char| c.is_whitespace() || c == ',')
            .any(path_like)
}

#[cfg(test)]
const CASES: [(&str, bool); 24] = [
    ("none", false),
    ("None.", false),
    ("no", false),
    ("nothing", false),
    ("n/a", false),
    ("N/A", false),
    ("no files changed", false),
    ("No changes needed", false),
    ("", false),
    ("(none)", false),
    ("-", false),
    ("`none`", false),
    ("**none**", false),
    ("read-only investigation", false),
    ("unchanged.", false),
    ("none (read the Node.js docs)", false),
    ("e.g. nothing", false),
    ("version 1.2 bumped", false),
    ("src/lib.rs", true),
    ("`src/lib.rs`, `src/api.rs`", true),
    ("**README.md**.", true),
    ("Cargo.toml and docs/status.md", true),
    ("updated crates/aq/src/x.rs (added a test)", true),
    ("Makefile.", false),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_changes_value_claims_changes_only_when_it_names_a_path() {
        for (value, claims) in CASES {
            assert_eq!(names_paths(value), claims, "{value:?}");
        }
    }
    #[test]
    fn conclusion_fields_are_read_through_case_and_markdown() {
        for (value, claims) in CASES {
            let conclusion =
                format!("Outcome: done; Changes: {value}; Copied: none; Claude exit 0");
            assert_eq!(claims_changes(&conclusion), claims, "{conclusion}");
        }
        assert!(claims_changes("Changes: src/lib.rs; Checks: cargo test"));
        assert!(!claims_changes("worker claims task is done; unverified"));
        assert!(!claims_changes("Changes: unknown; Runner failure: x"));
        assert!(!claims_changes("Result: see src/lib.rs; Changes: none"));
        for c in [
            "Outcome: blocked; Changes: none",
            "**outcome:** Blocked; Changes: none",
        ] {
            assert!(blocked(c), "{c}");
        }
        for c in [
            "Outcome: done; Changes: none",
            "Changes: none; Result: Outcome: blocked",
        ] {
            assert!(!blocked(c), "{c}");
        }
    }
}
