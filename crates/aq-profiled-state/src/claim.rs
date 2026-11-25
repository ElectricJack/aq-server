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
#[derive(Clone, Copy)]
enum ConclusionField {
    Changes,
    Outcome,
}
impl ConclusionField {
    fn leading(self, conclusion: &str) -> Option<&str> {
        let key = match self {
            Self::Changes => "changes",
            Self::Outcome => "outcome",
        };
        conclusion
            .split(';')
            .take(4)
            .find_map(|part| field(part, key))
    }
}
/// Review I-2, R1-I1: the conclusion claims changes (its `Changes:` value names a
/// path). An older worker writes the copied-back files there.
pub fn claims_changes(conclusion: &str) -> bool {
    ConclusionField::Changes
        .leading(conclusion)
        .is_some_and(names_paths)
}
/// Owner ruling 4a, R1-M2: the conclusion opens with `Outcome: blocked`.
pub fn blocked(conclusion: &str) -> bool {
    ConclusionField::Outcome
        .leading(conclusion)
        .is_some_and(said_blocked)
}
/// An `Outcome:` value whose first word is `blocked`.
pub fn said_blocked(value: &str) -> bool {
    value
        .split_whitespace()
        .map(|word| Token(word).bare().0)
        .find(|word| !word.is_empty())
        .is_some_and(|word| word.eq_ignore_ascii_case("blocked"))
}

struct Token<'a>(&'a str);
impl Token<'_> {
    /// A token without its wrapping backticks or emphasis and its trailing `.` `;` `,`.
    fn bare(self) -> Self {
        const WRAP: [char; 3] = ['*', '_', '`'];
        Self(
            self.0
                .trim_matches(WRAP)
                .trim_end_matches(['.', ';', ','])
                .trim_matches(WRAP),
        )
    }
    /// Review R3-I1: a path token is (a) a file name, (b) a slash path whose last
    /// segment is a file name, (c) a slash path ending in `/` whose first segment has
    /// two or more characters, or (d) a slash path with a segment containing `-`, `_`
    /// or a digit, or a segment that is a file or dot name. Letter-only slash words
    /// (`none/nil`, `and/or`, `CI/CD`, `I/O`) are not paths, and nothing with `://` is.
    fn is_path(&self) -> bool {
        let token = self.0;
        let allowed = |c| name_char(c) || c == '/';
        if token.contains("://") || !token.chars().all(allowed) {
            return false;
        }
        let Some((_, last)) = token.rsplit_once('/') else {
            return Token(token).file_name();
        };
        let directory =
            last.is_empty() && token.split('/').next().unwrap_or_default().chars().count() >= 2;
        Token(last).file_name() || directory || token.split('/').map(Token).any(Token::distinctive)
    }

    fn valid_stem(&self) -> bool {
        let first = self.0.split('.').next().unwrap_or_default();
        first.chars().count() >= 2 && self.0.chars().all(name_char) && !self.is_version()
    }
    fn is_version(&self) -> bool {
        self.0
            .trim_start_matches(['v', 'V'])
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
    }
    fn valid_extension(&self) -> bool {
        (1..=8).contains(&self.0.len())
            && self.0.chars().all(|c| c.is_ascii_alphanumeric())
            && self.0.chars().any(|c| c.is_ascii_alphabetic())
    }

    /// A file name `name.ext`: `ext` 1-8 alphanumerics with a letter; `name` at least two
    /// characters, starting with two before any dot (`a.k.a`, `e.g` are words), of
    /// letters, digits, `-`, `_` and `.`, and not a bare number or version.
    fn file_name(&self) -> bool {
        let Some((name, ext)) = self.0.rsplit_once('.') else {
            return false;
        };
        Token(name).valid_stem() && Token(ext).valid_extension()
    }
    /// A dotfile or dot-directory segment (`.github`, `.config`).
    fn dot_name(&self) -> bool {
        self.0
            .strip_prefix('.')
            .is_some_and(|rest| rest.chars().count() >= 2 && rest.chars().all(name_char))
    }
    fn distinctive(self) -> bool {
        let marked = self
            .0
            .chars()
            .any(|c| matches!(c, '-' | '_') || c.is_ascii_digit());
        marked || self.file_name() || self.dot_name()
    }
}
fn name_char(c: char) -> bool {
    c.is_alphanumeric() || "-_.".contains(c)
}

/// Review R2-I1, R3-I1: a `Changes:` value claims changes only when it consists
/// ENTIRELY of path tokens: separated by commas and/or whitespace, after a leading
/// list bullet, each optionally wrapped in backticks or emphasis and followed by a
/// trailing `.` `;` or `,`. Any other character or word (a parenthesised note, a
/// colon, a dash used as a separator, a word that is not a path) means no claim.
/// Prose is never interpreted: a missed claim only loses this consistency check
/// (the task then reads "Completed, no changes" and the full message is in its
/// comments), while a false claim fails an honest run.
pub fn names_paths(value: &str) -> bool {
    let value = value.trim();
    let value = ["- ", "* ", "+ "]
        .iter()
        .find_map(|b| value.strip_prefix(b))
        .unwrap_or(value);
    let mut tokens = value
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .peekable();
    tokens.peek().is_some() && tokens.all(|token| Token(token).bare().is_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    const CASES: &str = include_str!("claim_cases.txt");
    /// FNV-1a 64 of the shared table; the same constant is in the other repository.
    const CASES_FNV: u64 = 0xce2b9d1a7171d515;
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
