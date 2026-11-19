//! What the server checks about a Codex ChatGPT login (`auth.json`) before it
//! leases one or stores a node's write-back. Nothing here logs or returns a
//! token; refusals name only ages and limits.
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Provider id of Codex subscription logins in the credential registry.
pub const PROVIDER: &str = "openai";

/// The comparable facts of one well-formed ChatGPT login.
#[derive(Debug, PartialEq, Eq)]
pub struct Stamp {
    account: String,
    pub last_refresh: Option<DateTime<Utc>>,
}

/// None unless `bytes` is a well-formed ChatGPT login (an API-key login is not).
pub fn stamp(bytes: &[u8]) -> Option<Stamp> {
    #[derive(Deserialize)]
    struct Tokens {
        account_id: String,
    }
    #[derive(Deserialize)]
    struct Auth {
        tokens: Tokens,
        last_refresh: Option<DateTime<Utc>>,
    }
    aq_session_host::PrivateLaunchInput::validate_chatgpt_auth_json(bytes).ok()?;
    let auth: Auth = serde_json::from_slice(bytes).ok()?;
    Some(Stamp {
        account: auth.tokens.account_id,
        last_refresh: auth.last_refresh,
    })
}

/// Whether `new` is the `stored` login refreshed: both well-formed, the same
/// account, and `new` refreshed later (any dated refresh when `stored` has none).
pub fn refreshed(stored: &[u8], new: &[u8]) -> bool {
    let (Some(stored), Some(new)) = (stamp(stored), stamp(new)) else {
        return false;
    };
    stored.account == new.account
        && new
            .last_refresh
            .is_some_and(|at| stored.last_refresh.is_none_or(|old| at > old))
}

/// Why a login last refreshed more than `limit_days` ago is not leased; None
/// when it may be (no limit, within it, or a login that records no refresh).
pub fn stale(bytes: &[u8], limit_days: Option<i64>, now_ms: i64) -> Option<String> {
    let limit = limit_days?;
    let refreshed = stamp(bytes)?.last_refresh?.timestamp_millis();
    let days = (now_ms - refreshed) / 86_400_000;
    (days > limit).then(|| {
        format!(
            "codex_login_stale: stored Codex login is {days} days old (limit {limit}): import your current login"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn login(account: &str, refreshed: Option<&str>) -> Vec<u8> {
        serde_json::json!({"auth_mode": "chatgpt", "last_refresh": refreshed,
            "tokens": {"id_token": "i", "access_token": "a", "refresh_token": "r", "account_id": account}})
        .to_string()
        .into_bytes()
    }
    #[test]
    fn a_write_back_is_the_same_account_refreshed_later() {
        let stored = login("acct", Some("2026-11-12T15:12:29.436594182Z"));
        assert!(refreshed(
            &stored,
            &login("acct", Some("2026-11-18T22:00:00Z"))
        ));
        assert!(!refreshed(&stored, &stored), "not later");
        assert!(!refreshed(
            &stored,
            &login("acct", Some("2026-11-01T00:00:00Z"))
        ));
        assert!(!refreshed(&stored, &login("acct", None)));
        assert!(!refreshed(
            &stored,
            &login("other", Some("2026-11-18T22:00:00Z"))
        ));
        assert!(!refreshed(&stored, b"{not json"));
        assert!(!refreshed(
            &stored,
            br#"{"auth_mode":"apikey","OPENAI_API_KEY":"k"}"#
        ));
        assert!(!refreshed(
            b"opaque",
            &login("acct", Some("2026-11-18T22:00:00Z"))
        ));
        assert!(refreshed(
            &login("acct", None),
            &login("acct", Some("2026-11-18T22:00:00Z"))
        ));
        assert!(
            stamp(&login("acct", Some("yesterday"))).is_none(),
            "undated refresh"
        );
    }
    #[test]
    fn a_login_past_the_owner_limit_is_named_stale() {
        let now = "2026-11-18T00:00:00Z"
            .parse::<DateTime<Utc>>()
            .unwrap()
            .timestamp_millis();
        let old = login("acct", Some("2026-11-08T00:00:00Z"));
        assert_eq!(stale(&old, None, now), None);
        assert_eq!(stale(&old, Some(10), now), None);
        assert_eq!(
            stale(&old, Some(7), now).unwrap(),
            "codex_login_stale: stored Codex login is 10 days old (limit 7): import your current login"
        );
        assert_eq!(stale(&login("acct", None), Some(1), now), None);
        assert_eq!(stale(b"opaque", Some(1), now), None);
    }
}
