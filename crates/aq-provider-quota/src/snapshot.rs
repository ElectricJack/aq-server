//! Sanitized, host-refreshed subscription metadata. Never reads login files.
use super::*;

pub(super) fn read(source: &CodexSource, now: u64) -> Option<Collected> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(source.snapshot_file.as_ref()?)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.mode() & 0o022 != 0 || meta.len() > MAX_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_BYTES {
        return None;
    }
    let root: Value = serde_json::from_slice(&bytes).ok()?;
    if root.get("schema_version")?.as_u64()? != 1 {
        return None;
    }
    let entries = root.get("accounts")?.as_array()?;
    if entries.len() > 32 {
        return None;
    }
    let matches = entries
        .iter()
        .filter(|entry| {
            entry["provider"] == source.provider_id && entry["account_ref"] == source.account_ref
        })
        .collect::<Vec<_>>();
    let [account] = matches.as_slice() else {
        return None;
    };
    let observed = account.get("observed_at_ms")?.as_u64()?;
    if observed == 0 || observed > now.checked_add(5_000)? {
        return None;
    }
    let values = account.get("windows")?.as_array()?;
    if values.is_empty() || values.len() > 16 {
        return None;
    }
    let mut ids = BTreeSet::new();
    let mut windows = Vec::new();
    for value in values {
        let id = value.get("id")?.as_str()?;
        let label = value.get("label")?.as_str()?;
        if !valid_text(id, 128) || !valid_text(label, 128) || !ids.insert(id) {
            return None;
        }
        let mut w = window(id.into(), label.into(), value, observed)?;
        w.observation.source = match source.provider_id.as_str() {
            "openai" => "codex_app_server_rate_limits",
            "anthropic" if account["source"] == "claude_cli_usage_cache" => {
                "claude_cli_usage_cache"
            }
            "anthropic" => "claude_sdk_usage",
            _ => return None,
        }
        .into();
        w.observation.fresh_until = time(observed.checked_add(120_000)?)?;
        windows.push(w);
    }
    let reset_credits = Some(reset_credits(
        account.get("reset_credits"),
        observed,
        now,
        &source.provider_id,
        account,
    )?);
    Some(Collected {
        windows,
        reset_credits,
    })
}

fn reset_credits(
    value: Option<&Value>,
    fallback_observed: u64,
    now: u64,
    provider: &str,
    account: &Value,
) -> Option<ResetCredits> {
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
        let parse_grants = || -> Option<Option<Vec<ResetGrant>>> {
            let grants = match value.get("grants")? {
                Value::Null => None,
                Value::Array(values) if values.len() <= 100 => {
                    let mut grants = Vec::new();
                    for grant in values {
                        let label = grant.get("label")?.as_str()?;
                        if !valid_text(label, 120) {
                            return None;
                        }
                        let remaining_count = count(grant.get("remaining_count")?)?;
                        let expires_at = match grant.get("expires_at")? {
                            Value::Null => None,
                            Value::String(s)
                                if s.len() <= 64 && DateTime::parse_from_rfc3339(s).is_ok() =>
                            {
                                Some(Timestamp(s.clone()))
                            }
                            _ => return None,
                        };
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
                        grants.push(ResetGrant {
                            label: label.into(),
                            remaining_count,
                            expires_at,
                            usable_now,
                            clears,
                        });
                    }
                    Some(grants)
                }
                _ => return None,
            };
            Some(grants)
        };
        let grants = parse_grants().unwrap_or(None);
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
    let ParsedReset {
        state,
        available_count,
        grants,
        observed,
    } = value.and_then(|v| parse(v, now)).unwrap_or(ParsedReset {
        state: ResetCreditState::Unknown,
        available_count: None,
        grants: None,
        observed: fallback_observed,
    });
    let known = matches!(state, ResetCreditState::Known);
    let mut observation = observation(observed, known)?;
    observation.source = match provider {
        "openai" => "codex_app_server_rate_limits",
        "anthropic" if account["source"] == "claude_cli_usage_cache" => "claude_cli_usage_cache",
        "anthropic" => "claude_sdk_usage",
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn reset_metadata_keeps_counts_and_independent_observation() {
        let now = 1_800_000_000_000;
        let account = json!({});
        let value = json!({"state":"known", "available_count":2, "grants":[{
            "label":"Weekly", "remaining_count":1, "expires_at":"2026-12-01T00:00:00.000Z",
            "usable_now":false, "clears":["seven_day"]}], "observed_at_ms":now-300_000});
        let reset = reset_credits(Some(&value), now, now, "openai", &account).unwrap();
        assert!(matches!(reset.state, ResetCreditState::Known));
        assert_eq!(reset.available_count, Some(2));
        assert_eq!(reset.grants.unwrap()[0].usable_now, Some(false));
        assert_eq!(
            reset.observation.observed_at.0,
            time(now - 300_000).unwrap().0
        );
        let mut bad_grant = value.clone();
        bad_grant["grants"][0]["expires_at"] = json!("invalid");
        let reset = reset_credits(Some(&bad_grant), now, now, "openai", &account).unwrap();
        assert_eq!(reset.available_count, Some(2));
        assert!(reset.grants.is_none());
        let mut bad_count = value;
        bad_count["available_count"] = json!(-1);
        let reset = reset_credits(Some(&bad_count), now, now, "openai", &account).unwrap();
        assert!(matches!(reset.state, ResetCreditState::Unknown));
        assert!(reset.available_count.is_none());
        let missing = reset_credits(None, now, now, "openai", &account).unwrap();
        assert!(matches!(missing.state, ResetCreditState::Unknown));
        let zero = json!({"state":"known", "available_count":0,"grants":[],"observed_at_ms":now});
        let zero = reset_credits(Some(&zero), now, now, "openai", &account).unwrap();
        assert_eq!(zero.available_count, Some(0));
        assert_eq!(zero.grants.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn malformed_metadata_does_not_discard_windows_or_leak_between_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let now = 1_800_000_000_000;
        let source = |name| {
            serde_json::from_value::<CodexSource>(
                json!({"provider_id":"openai","account_ref":name,"snapshot_file":path}),
            )
            .unwrap()
        };
        let data = json!({"schema_version":1,"accounts":[
            {"provider":"openai","account_ref":"a","observed_at_ms":now,"windows":[{"id":"weekly","label":"Weekly","used_percent":10}],"reset_credits":{"state":"known","available_count":4,"grants":null,"observed_at_ms":now}},
            {"provider":"openai","account_ref":"b","observed_at_ms":now,"windows":[{"id":"weekly","label":"Weekly","used_percent":20}],"reset_credits":{"state":"known","available_count":1,"grants":null,"observed_at_ms":"bad"}}
        ]});
        std::fs::write(&path, data.to_string()).unwrap();
        let result = Collector::new()
            .collect(vec![], &[source("a"), source("b")], now)
            .await
            .0;
        assert_eq!(
            result[0].reset_credits.as_ref().unwrap().available_count,
            Some(4)
        );
        assert_eq!(result[1].windows[0].used_percent, Some(20.));
        assert!(matches!(
            result[1].reset_credits.as_ref().unwrap().state,
            ResetCreditState::Unknown
        ));
    }

    #[tokio::test]
    async fn omission_replaces_old_count_and_failed_read_keeps_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let source: CodexSource = serde_json::from_value(
            json!({"provider_id":"openai","account_ref":"a","snapshot_file":path}),
        )
        .unwrap();
        let now = 1_800_000_000_000;
        let mut data = json!({"schema_version":1,"accounts":[{"provider":"openai","account_ref":"a","observed_at_ms":now,"windows":[{"id":"weekly","label":"Weekly","used_percent":10}],"reset_credits":{"state":"known","available_count":3,"grants":null,"observed_at_ms":now-60_000}}]});
        std::fs::write(&path, data.to_string()).unwrap();
        let c = Collector::new();
        let first = c
            .collect(vec![], std::slice::from_ref(&source), now)
            .await
            .0;
        assert_eq!(
            first[0].reset_credits.as_ref().unwrap().available_count,
            Some(3)
        );
        std::fs::remove_file(&path).unwrap();
        let failed = c
            .collect(vec![], std::slice::from_ref(&source), now + 31_000)
            .await;
        assert!(failed.1);
        assert_eq!(
            failed.0[0]
                .reset_credits
                .as_ref()
                .unwrap()
                .observation
                .observed_at
                .0,
            time(now - 60_000).unwrap().0
        );
        data["accounts"][0]
            .as_object_mut()
            .unwrap()
            .remove("reset_credits");
        data["accounts"][0]["observed_at_ms"] = json!(now + 40_000);
        std::fs::write(&path, data.to_string()).unwrap();
        let later = c.collect(vec![], &[source], now + 40_000).await.0;
        assert!(matches!(
            later[0].reset_credits.as_ref().unwrap().state,
            ResetCreditState::Unknown
        ));
        assert_eq!(later[0].windows[0].used_percent, Some(10.));
    }

    #[tokio::test]
    async fn snapshot_accounts_are_visible_without_runtime_credentials_and_retain_freshness() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let source: CodexSource = serde_json::from_value(
            json!({"provider_id":"openai","account_ref":"Codex ChatGPT","snapshot_file":path}),
        )
        .unwrap();
        let now = 1_800_000_000_000;
        let mut data = json!({"schema_version":1,"accounts":[{"provider":"openai","account_ref":"Codex ChatGPT","observed_at_ms":now,"windows":[{"id":"weekly","label":"Weekly","used_percent":52,"reset_at":1800000001}]}]});
        std::fs::write(&path, data.to_string()).unwrap();
        let c = Collector::new();
        let (values, _) = c.collect(vec![], std::slice::from_ref(&source), now).await;
        assert_eq!(values[0].windows[0].used_percent, Some(52.));
        let until = values[0].windows[0].observation.fresh_until.0.clone();
        let (later, _) = c
            .collect(vec![], std::slice::from_ref(&source), now + 180_000)
            .await;
        assert_eq!(later[0].windows[0].observation.fresh_until.0, until);
        data["accounts"][0]["windows"][0]["used_percent"] = json!(101);
        std::fs::write(&path, data.to_string()).unwrap();
        let (stale, partial) = c
            .collect(vec![], std::slice::from_ref(&source), now + 240_000)
            .await;
        assert!(partial);
        assert_eq!(stale[0].windows[0].used_percent, Some(52.));
        assert!(
            Collector::new().collect(vec![], &[source], now).await.0[0].windows[0]
                .used_percent
                .is_none()
        );
    }
}
