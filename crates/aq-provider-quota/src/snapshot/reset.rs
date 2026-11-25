use super::*;

struct ParsedReset {
    state: ResetCreditState,
    available_count: Option<u32>,
    grants: Option<Vec<ResetGrant>>,
    observed: u64,
}

fn count(value: &Value) -> Option<Option<u32>> {
    if value.is_null() {
        return Some(None);
    }
    let n = value.as_u64()?;
    (n <= 1_000_000).then_some(Some(n as u32))
}
fn parse(value: &Value, now: u64) -> Option<ParsedReset> {
    let state = match value.get("state")?.as_str()? {
        "known" => ResetCreditState::Known,
        "unknown" => ResetCreditState::Unknown,
        "unavailable" => ResetCreditState::Unavailable,
        _ => return None,
    };
    let available = count(value.get("available_count")?)?;
    if matches!(state, ResetCreditState::Known) != available.is_some() {
        return None;
    }
    let grants = value.get("grants").and_then(parse_grants);
    let observed = value.get("observed_at_ms")?.as_u64()?;
    if observed == 0 || observed > now.checked_add(5_000)? {
        return None;
    }
    Some(ParsedReset {
        state,
        available_count: available,
        grants,
        observed,
    })
}
fn parse_grants(value: &Value) -> Option<Vec<ResetGrant>> {
    let grants = value.as_array()?;
    if grants.len() > 100 {
        return None;
    }
    grants.iter().map(parse_grant).collect()
}
fn parse_grant(grant: &Value) -> Option<ResetGrant> {
    let label = grant.get("label")?.as_str()?;
    if !valid_text(label, 120) {
        return None;
    }
    let remaining_count = count(grant.get("remaining_count")?)?;
    let expires_at = expires_at(grant.get("expires_at")?)?;
    let usable_now = match grant.get("usable_now")? {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        _ => return None,
    };
    let clears = grant.get("clears")?.as_array()?;
    if clears.len() > 16 {
        return None;
    }
    let clears = clears
        .iter()
        .map(|v| v.as_str().filter(|s| valid_text(s, 80)).map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    Some(ResetGrant {
        label: label.into(),
        remaining_count,
        expires_at,
        usable_now,
        clears,
    })
}
fn expires_at(value: &Value) -> Option<Option<Timestamp>> {
    let at = match value {
        Value::Null => None,
        Value::String(s) if s.len() <= 64 => {
            let date = DateTime::parse_from_rfc3339(s).ok()?;
            let normalized = date
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true);
            // Timestamp's wire schema requires a four-digit UTC year.
            if normalized.len() != 24 {
                return None;
            }
            Some(Timestamp(normalized))
        }
        _ => return None,
    };
    Some(at)
}
pub(super) fn reset_credits(
    value: Option<&Value>,
    fallback_observed: u64,
    now: u64,
    provider: &str,
) -> Option<ResetCredits> {
    let parsed = value.and_then(|v| parse(v, now));
    let has_reset_observation = parsed.is_some();
    let ParsedReset {
        state,
        available_count,
        grants,
        observed,
    } = parsed.unwrap_or(ParsedReset {
        state: ResetCreditState::Unknown,
        available_count: None,
        grants: None,
        observed: fallback_observed,
    });
    let known = matches!(state, ResetCreditState::Known);
    let mut observation = observation(observed, known)?;
    observation.source = match (provider, has_reset_observation) {
        ("openai", true) => "codex_app_server_rate_limits",
        // CLI cache supplies quota windows only; any retained reset block was
        // observed independently through the SDK.
        ("anthropic", true) => "claude_sdk_usage",
        ("openai" | "anthropic", false) => "provider_quota_unknown",
        _ => return None,
    }
    .into();
    observation.fresh_until = time(observed.checked_add(120_000)?)?;
    Some(ResetCredits {
        state,
        available_count,
        grants,
        observation,
    })
}
