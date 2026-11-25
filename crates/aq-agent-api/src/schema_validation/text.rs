use super::within_length;
use serde_json::Value;

pub(super) fn check_text(schema: &Value, text: &str) -> bool {
    if !within_length(
        text.chars().count(),
        &schema["minLength"],
        &schema["maxLength"],
    ) {
        return false;
    }
    if schema["x-utf8-maxBytes"]
        .as_u64()
        .is_some_and(|limit| text.len() > limit as usize)
    {
        return false;
    }
    if schema["x-int64-token"] == true && !positive_int64(text) {
        return false;
    }
    let pattern_matches = schema["pattern"]
        .as_str()
        .is_none_or(|pattern| matches_pattern(pattern, text));
    pattern_matches
        && match schema["format"].as_str() {
            Some("uuid") => uuid::Uuid::parse_str(text).is_ok_and(|u| u.to_string() == text),
            Some("date-time") => chrono::DateTime::parse_from_rfc3339(text).is_ok(),
            _ => true,
        }
}

fn lower_hex(c: u8) -> bool {
    c.is_ascii_digit() || (b'a'..=b'f').contains(&c)
}
/// `^[1-9][0-9]{0,18}$`.
fn positive_token(t: &str) -> bool {
    !t.starts_with('0') && (1..=19).contains(&t.len()) && t.bytes().all(|c| c.is_ascii_digit())
}

fn positive_int64(text: &str) -> bool {
    positive_token(text) && text.parse::<i64>().is_ok_and(|n| n > 0)
}

fn hex_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(lower_hex)
}
fn reply_cursor(text: &str) -> bool {
    text.split_once('.')
        .is_some_and(|(owner, reply)| positive_token(owner) && positive_token(reply))
}

fn matches_pattern(pattern: &str, text: &str) -> bool {
    match pattern {
        "^https?://" => text.starts_with("https://") || text.starts_with("http://"),
        "\\S" => text.chars().any(|c| !c.is_whitespace()),
        "^[1-9][0-9]{0,18}$" => positive_token(text),
        "^(0|[1-9][0-9]{0,18})$" => text == "0" || positive_token(text),
        "^[1-9][0-9]{0,18}\\.[1-9][0-9]{0,18}$" => reply_cursor(text),
        "^[1-9][0-9]{0,18}(\\.[1-9][0-9]{0,18})?$" => positive_token(text) || reply_cursor(text),
        _ => matches_identifier_pattern(pattern, text),
    }
}

fn matches_identifier_pattern(pattern: &str, text: &str) -> bool {
    match pattern {
        "^([0-9a-f]{40}|[0-9a-f]{64})$" => {
            matches!(text.len(), 40 | 64) && text.bytes().all(lower_hex)
        }
        "^sha256:[0-9a-f]{64}$" => text.strip_prefix("sha256:").is_some_and(hex_digest),
        "^[0-9a-f]{64}$" => hex_digest(text),
        "^[0-9a-f-]{36}$" => text.len() == 36 && text.bytes().all(|c| lower_hex(c) || c == b'-'),
        _ => false,
    }
}
