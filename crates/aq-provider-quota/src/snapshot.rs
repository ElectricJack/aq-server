//! Sanitized, host-refreshed subscription metadata. Never reads login files.
use super::*;
mod reset;
use reset::reset_credits;

pub(super) fn read(source: &CodexSource, now: u64) -> Option<Collected> {
    let root = read_snapshot(source.snapshot_file.as_ref()?)?;
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
    let windows = account_windows(account, &source.provider_id, observed)?;
    let reset_credits = Some(reset_credits(
        account.get("reset_credits"),
        observed,
        now,
        &source.provider_id,
    )?);
    Some(Collected {
        windows,
        reset_credits,
    })
}

fn read_snapshot(path: &std::path::Path) -> Option<Value> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    let safe_file = meta.is_file() && meta.mode() & 0o022 == 0;
    if !safe_file || meta.len() > MAX_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
fn account_windows(account: &Value, provider: &str, observed: u64) -> Option<Vec<QuotaWindow>> {
    let values = account.get("windows")?.as_array()?;
    if values.is_empty() || values.len() > 16 {
        return None;
    }
    let name = observation_source(account, provider)?;
    let mut ids = BTreeSet::new();
    let mut windows = Vec::new();
    for value in values {
        let id = value.get("id")?.as_str()?;
        let label = value.get("label")?.as_str()?;
        let valid_names = valid_text(id, 128) && valid_text(label, 128);
        if !valid_names || !ids.insert(id) {
            return None;
        }
        let mut w = window(id.into(), label.into(), value, observed)?;
        w.observation.source = name.into();
        w.observation.fresh_until = time(observed.checked_add(120_000)?)?;
        windows.push(w);
    }
    Some(windows)
}
fn observation_source(account: &Value, provider: &str) -> Option<&'static str> {
    let name = match provider {
        "openai" => "codex_app_server_rate_limits",
        "anthropic" if account["source"] == "claude_cli_usage_cache" => "claude_cli_usage_cache",
        "anthropic" => "claude_sdk_usage",
        _ => return None,
    };
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn reset_metadata_keeps_counts_and_independent_observation() {
        let now = 1_800_000_000_000;
        let value = json!({"state":"known", "available_count":2, "grants":[{
            "label":"Weekly", "remaining_count":1, "expires_at":"2026-12-01T00:00:00.000Z",
            "usable_now":false, "clears":["seven_day"]}], "observed_at_ms":now-300_000});
        let reset = reset_credits(Some(&value), now, now, "openai").unwrap();
        assert!(matches!(reset.state, ResetCreditState::Known));
        assert_eq!(reset.available_count, Some(2));
        assert_eq!(reset.grants.unwrap()[0].usable_now, Some(false));
        assert_eq!(
            reset.observation.observed_at.0,
            time(now - 300_000).unwrap().0
        );
        let mut offset = value.clone();
        offset["grants"][0]["expires_at"] = json!("2026-12-01T05:30:00+05:30");
        let normalized = reset_credits(Some(&offset), now, now, "openai").unwrap();
        assert_eq!(
            normalized.grants.unwrap()[0].expires_at.as_ref().unwrap().0,
            "2026-12-01T00:00:00.000Z"
        );
        let mut bad_grant = value.clone();
        bad_grant["grants"][0]["expires_at"] = json!("invalid");
        let reset = reset_credits(Some(&bad_grant), now, now, "openai").unwrap();
        assert_eq!(reset.available_count, Some(2));
        assert!(reset.grants.is_none());
        let mut bad_count = value;
        bad_count["available_count"] = json!(-1);
        let reset = reset_credits(Some(&bad_count), now, now, "openai").unwrap();
        assert!(matches!(reset.state, ResetCreditState::Unknown));
        assert!(reset.available_count.is_none());
        let missing = reset_credits(None, now, now, "openai").unwrap();
        assert!(matches!(missing.state, ResetCreditState::Unknown));
        let zero = json!({"state":"known", "available_count":0,"grants":[],"observed_at_ms":now});
        let zero = reset_credits(Some(&zero), now, now, "openai").unwrap();
        assert_eq!(zero.available_count, Some(0));
        assert_eq!(zero.grants.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn cached_claude_windows_keep_reset_sdk_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let now = 1_800_000_000_000;
        let source: CodexSource = serde_json::from_value(
            json!({"provider_id":"anthropic","account_ref":"a","snapshot_file":path}),
        )
        .unwrap();
        let mut data = json!({"schema_version":1,"accounts":[{"provider":"anthropic","account_ref":"a","source":"claude_cli_usage_cache","observed_at_ms":now,"windows":[{"id":"weekly","label":"Weekly","used_percent":10}],"reset_credits":{"state":"unknown","available_count":null,"grants":null,"observed_at_ms":now-300_000}}]});
        std::fs::write(&path, data.to_string()).unwrap();
        let result = Collector::new()
            .collect(vec![], std::slice::from_ref(&source), now)
            .await
            .0;
        assert_eq!(
            result[0].windows[0].observation.source,
            "claude_cli_usage_cache"
        );
        let reset = result[0].reset_credits.as_ref().unwrap();
        assert_eq!(reset.observation.source, "claude_sdk_usage");
        assert_eq!(
            reset.observation.observed_at.0,
            time(now - 300_000).unwrap().0
        );
        data["accounts"][0]
            .as_object_mut()
            .unwrap()
            .remove("reset_credits");
        std::fs::write(&path, data.to_string()).unwrap();
        let result = Collector::new().collect(vec![], &[source], now).await.0;
        assert_eq!(
            result[0].reset_credits.as_ref().unwrap().observation.source,
            "provider_quota_unknown"
        );
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
