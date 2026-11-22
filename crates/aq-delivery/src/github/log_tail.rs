//! The tail of a failed job's log, made safe to keep and show (spec 0021
//! section 5.3): control sequences removed, credentials redacted, and the
//! whole bounded by the `check_log_bytes` Setting. Mechanism only; the
//! patterns are the credential scanner's (provider keys, JWTs, bearer
//! tokens, `Authorization` headers, private key blocks, credential
//! assignments), written without a regex engine.

const REDACTED: &str = "[redacted]";

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

/// Substrings of an assignment's name that mark its value as a credential.
const SECRET_NAMES: [&str; 9] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "private_key",
];

/// The tail of a log, from the last bytes of it that were read.
///
/// `window` holds at most the last `max + 1` bytes of the log: it holding more
/// than `max` bytes means earlier bytes were cut, so the first line (possibly
/// partial, possibly the end of a key) is dropped. The result is sanitized,
/// redacted and at most `max` bytes; `None` when nothing is left.
pub(super) fn log_tail(window: &[u8], max: usize) -> Option<String> {
    if max == 0 {
        return None;
    }
    let text = String::from_utf8_lossy(window);
    let mut text: &str = &text;
    if window.len() > max {
        text = text.split_once('\n').map_or("", |(_, rest)| rest);
    }
    let redacted = redact(&strip_controls(text));
    let kept = fit(&redacted, max).trim().to_owned();
    (!kept.is_empty()).then_some(kept)
}

/// `text` cut from the front to at most `max` bytes: whole lines while there
/// are any to drop, then at a character boundary.
fn fit(text: &str, max: usize) -> &str {
    let mut text = text.trim_end();
    while text.len() > max {
        let Some(newline) = text.find('\n') else {
            let mut start = text.len() - max;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            return &text[start..];
        };
        text = &text[newline + 1..];
    }
    text
}

/// Removes ANSI escape sequences and every control character but newline and
/// tab (a carriage return only ends a progress line).
fn strip_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    // Parameters and intermediates, then one final byte.
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
            }
            '\n' | '\t' => out.push(c),
            '\r' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Replaces credentials in `text` with `[redacted]`.
pub(super) fn redact(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_key_block = false;
    for line in text.split('\n') {
        if in_key_block {
            in_key_block = !line.contains("-----END");
            continue;
        }
        if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
            in_key_block = !line.contains("-----END");
            out.push("[redacted private key]".to_owned());
            continue;
        }
        out.push(redact_line(line));
    }
    out.join("\n")
}

fn redact_line(line: &str) -> String {
    let line = redact_authorization(line);
    let line = redact_assignments(&line);
    redact_tokens(&line)
}

/// An `Authorization` header or assignment loses the rest of its line.
fn redact_authorization(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find("authorization") {
        let name_end = from + at + "authorization".len();
        let rest = &lower[name_end..];
        let skipped = rest.len() - rest.trim_start_matches(['"', '\'', ' ', '\t']).len();
        if rest[skipped..].starts_with([':', '=']) {
            let end = name_end + skipped + 1;
            return format!("{} {REDACTED}", &line[..end]);
        }
        from = name_end;
    }
    line.to_owned()
}

/// `name = value` where the name contains a secret word: the value is redacted.
fn redact_assignments(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut at = 0;
    while at < line.len() {
        let hit = SECRET_NAMES
            .iter()
            .filter_map(|name| lower[at..].find(name).map(|i| (at + i, name.len())))
            .min();
        let Some((start, len)) = hit else { break };
        // The rest of the name, a closing quote, then the separator.
        let mut i = start + len;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || b"_-".contains(&bytes[i])) {
            i += 1;
        }
        let name_end = i;
        while i < bytes.len() && b"\"' \t".contains(&bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() || !b"=:".contains(&bytes[i]) {
            out.push_str(&line[at..name_end]);
            at = name_end;
            continue;
        }
        i += 1;
        while i < bytes.len() && b" \t\"'".contains(&bytes[i]) {
            i += 1;
        }
        let value_start = i;
        while i < bytes.len() && !b" \t\"'&,;)".contains(&bytes[i]) {
            i += 1;
        }
        out.push_str(&line[at..value_start]);
        if i > value_start && &line[value_start..i] != REDACTED {
            out.push_str(REDACTED);
        } else {
            out.push_str(&line[value_start..i]);
        }
        at = i;
    }
    out.push_str(&line[at..]);
    out
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Provider keys, JWTs and the token after `Bearer`.
fn redact_tokens(line: &str) -> String {
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
    if KEY_PREFIXES
        .iter()
        .any(|p| segment.starts_with(p) && segment.len() >= p.len().max(MIN_TOKEN))
    {
        return true;
    }
    // AWS access key id.
    if segment.len() == 20
        && segment.starts_with("AKIA")
        && segment
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return true;
    }
    // A JWT: three dot-separated parts, the first a base64 JSON header.
    segment.starts_with("eyJ") && segment.matches('.').count() == 2 && segment.len() >= 24
}

#[cfg(test)]
mod tests {
    use super::*;

    // Credential-shaped values are assembled here so no literal in the source
    // looks like one.
    fn key() -> String {
        format!("{}{}", "sk-", "A1b2C3d4E5f6G7h8I9j0K1l2")
    }

    #[test]
    fn provider_keys_bearer_tokens_and_authorization_headers_are_replaced() {
        let key = key();
        let github = format!("{}{}", "ghp_", "0123456789abcdefghijABCDEFGHIJ");
        let jwt = format!(
            "{}.{}.{}",
            "eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiJ4In0", "c2ln"
        );
        let text = format!(
            "calling with {key} now\nAuthorization: Bearer abcdef0123456789\n\
             curl -H \"authorization: token {github}\"\nuse {jwt} here\n\
             Bearer 9f8e7d6c5b4a3210 trailing\nfine line sk-short"
        );
        let out = redact(&text);
        for secret in [
            key.as_str(),
            &github,
            &jwt,
            "abcdef0123456789",
            "9f8e7d6c5b4a3210",
        ] {
            assert!(!out.contains(secret), "{secret} in {out}");
        }
        assert!(out.contains("Authorization: [redacted]"), "{out}");
        assert!(out.contains("fine line sk-short"), "short ids stay: {out}");
        assert!(out.contains("calling with [redacted] now"), "{out}");
    }

    #[test]
    fn credential_assignments_keep_their_name_and_lose_the_value() {
        let out = redact(
            "PASSWORD=hunter2hunter2 and api_key: \"abc123xyz\" then client_secret = s3cr3t,\nTOKEN_FILE ok",
        );
        assert_eq!(
            out,
            "PASSWORD=[redacted] and api_key: \"[redacted]\" then client_secret = [redacted],\nTOKEN_FILE ok"
        );
        assert!(!out.contains("hunter2"));
    }

    #[test]
    fn a_private_key_block_is_replaced_as_a_whole() {
        let begin = format!("-----BEGIN RSA {} KEY-----", "PRIVATE");
        let out = redact(&format!(
            "before\n{begin}\nMIIEvQIBADANBgkq\n-----END RSA PRIVATE KEY-----\nafter"
        ));
        assert_eq!(out, "before\n[redacted private key]\nafter");
    }

    #[test]
    fn the_tail_is_the_last_bytes_whole_lines_only_and_never_over_the_bound() {
        let log: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
        let max = 100;
        let from = log.len().saturating_sub(max + 1);
        let tail = log_tail(&log.as_bytes()[from..], max).unwrap();
        assert!(tail.len() <= max, "{}", tail.len());
        assert!(tail.ends_with("line 199"), "{tail}");
        assert!(tail.starts_with("line "), "no partial first line: {tail}");
        // A short log is kept whole.
        assert_eq!(log_tail(b"a\nb\n", 100).as_deref(), Some("a\nb"));
        assert_eq!(log_tail(b"", 100), None);
    }

    #[test]
    fn redaction_that_grows_the_text_still_fits_the_bound() {
        let log = "password=x\n".repeat(40);
        let tail = log_tail(log.as_bytes(), 120).unwrap();
        assert!(tail.len() <= 120, "{}", tail.len());
        assert!(tail.contains("password=[redacted]"), "{tail}");
    }

    #[test]
    fn a_cut_inside_a_key_leaves_none_of_it() {
        let key = key();
        let log = format!("{key}\nlast line\n");
        // The window starts inside the key's line.
        let window = &log.as_bytes()[8..];
        let tail = log_tail(window, window.len() - 1).unwrap();
        assert_eq!(tail, "last line");
    }

    #[test]
    fn control_sequences_and_multibyte_characters_are_handled() {
        let tail = log_tail("\u{1b}[31mred\u{1b}[0m\r\nüñí\tok\u{7}".as_bytes(), 100).unwrap();
        assert_eq!(tail, "red\nüñí\tok");
        let long = format!("a\n{}", "é".repeat(100));
        assert!(log_tail(long.as_bytes(), 51).unwrap().len() <= 51);
    }
}
