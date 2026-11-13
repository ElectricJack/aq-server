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
        .trim_matches(['*', '_', '`', '\'', '"', '[', ']', '<', '>', '{', '}'])
        .trim_end_matches(['.', ',', ':', ';', '!', '?'])
        .trim_matches(['*', '_', '`', '\'', '"', '[', ']'])
}
fn first_word(value: &str) -> &str {
    value
        .split_whitespace()
        .map(bare)
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}
/// A token that names a file or directory: no `://`, and either `dir/name` (a
/// segment of at least two characters) or `name.ext` (ext: 1-8 alphanumerics
/// with a letter; name at least two characters and not a bare version or number).
/// The two-character minimums keep `n/a`, `w/o`, `n.a` and `e.g` words.
fn path_like(token: &str) -> bool {
    if token.contains("://") {
        return false;
    }
    if token.contains('/') {
        // A segment of two or more characters: `n/a` and `w/o` are words, not paths.
        return token.split('/').any(|s| s.chars().count() >= 2);
    }
    let Some((name, ext)) = token.rsplit_once('.') else {
        return false;
    };
    let number = |s: &str| {
        s.trim_start_matches(['v', 'V'])
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
    };
    // Two or more characters before the dot: `n.a` and `e.g` are words.
    name.chars().count() >= 2
        && !number(name)
        && (1..=8).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}

/// Review R2-I1: a `Changes:` value claims changes only when, after removing
/// markdown emphasis, backticks, a list bullet and trailing punctuation, it is
/// ENTIRELY one or more path-like tokens separated by commas and/or whitespace
/// (each optionally followed by a parenthesised note, which is ignored). Any
/// other word makes it no claim. Prose is never interpreted: a missed claim only
/// loses this consistency check (the task then reads "Completed, no changes" and
/// the full message is in its comments), while a false claim fails an honest run.
pub fn names_paths(value: &str) -> bool {
    let value = value.trim().trim_start_matches(['-', '+', '>', ' ']);
    let mut rest = String::new();
    let mut depth = 0usize;
    for c in value.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => rest.push(c),
            _ => {}
        }
    }
    if depth > 0 {
        return false;
    }
    let tokens: Vec<&str> = rest
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(bare)
        .filter(|t| !t.is_empty())
        .collect();
    !tokens.is_empty() && tokens.iter().all(|t| path_like(t))
}

#[cfg(test)]
mod tests {
    use super::*;
    const CASES: &str = include_str!("claim_cases.txt");
    /// FNV-1a 64 of the shared table; the same constant is in the other repository.
    const CASES_FNV: u64 = 0xd6f4c4de7281c88d;
    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        })
    }
    fn cases() -> Vec<(bool, &'static str)> {
        CASES
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| {
                let (kind, value) = l.split_once('\t').expect("kind<TAB>value");
                assert!(kind == "claim" || kind == "none", "{l}");
                (kind == "claim", value)
            })
            .collect()
    }
    #[test]
    fn the_shared_claim_table_is_unchanged() {
        assert_eq!(
            fnv(CASES.as_bytes()),
            CASES_FNV,
            "update both repositories' tables together"
        );
        assert!(cases().len() >= 60);
    }
    #[test]
    fn a_changes_value_claims_changes_only_when_it_is_a_list_of_paths() {
        for (claims, value) in cases() {
            assert_eq!(names_paths(value), claims, "{value:?}");
        }
    }
    #[test]
    fn conclusion_fields_are_read_through_case_and_markdown() {
        for (claims, value) in cases() {
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
