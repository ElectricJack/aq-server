//! Sanitized, host-refreshed subscription metadata. Never reads login files.
use super::*;

pub(super) fn read(source: &CodexSource, now: u64) -> Option<Vec<QuotaWindow>> {
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
    Some(windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
