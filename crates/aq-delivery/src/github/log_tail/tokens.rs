use super::REDACTED;
/// Token prefixes: a segment starting with one, and at least `MIN_TOKEN` bytes
/// long, is a provider key.
const KEY_PREFIXES: [&str; 12] = [
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxa-",
    "xoxp-",
    "xoxs-",
];
const MIN_TOKEN: usize = 16;
fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Provider keys, JWTs and the token after `Bearer`.
pub(super) fn redact(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut previous = String::new();
    let mut rest = line;
    while !rest.is_empty() {
        let end = rest
            .char_indices()
            .find(|(_, c)| !token_char(*c))
            .map_or(rest.len(), |(i, _)| i);
        if end == 0 {
            let c = rest.chars().next().expect("nonempty");
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let segment = &rest[..end];
        let after_bearer = previous.eq_ignore_ascii_case("bearer") && segment.len() >= 8;
        if after_bearer || is_token(segment) {
            out.push_str(REDACTED);
        } else {
            out.push_str(segment);
        }
        previous = segment.to_owned();
        rest = &rest[end..];
    }
    out
}

fn is_token(segment: &str) -> bool {
    provider_key(segment) || aws_key(segment) || jwt(segment)
}
fn provider_key(segment: &str) -> bool {
    segment.len() >= MIN_TOKEN
        && KEY_PREFIXES
            .iter()
            .any(|prefix| segment.starts_with(prefix))
}
fn aws_key(segment: &str) -> bool {
    segment.len() == 20
        && segment.starts_with("AKIA")
        && segment
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}
fn jwt(segment: &str) -> bool {
    segment.len() >= 24 && segment.starts_with("eyJ") && segment.matches('.').count() == 2
}
