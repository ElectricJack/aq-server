use super::*;

pub(super) fn parse(body: &[u8], expected_account: &str, now: u64) -> Option<Vec<QuotaWindow>> {
    let root: Value = serde_json::from_slice(body).ok()?;
    let o = root.as_object()?;
    if let Some(account) = o.get("account_id").filter(|v| !v.is_null())
        && account.as_str()? != expected_account
    {
        return None;
    }
    let mut windows = Vec::new();
    let main = o.get("rate_limit").unwrap_or(&Value::Null);
    append_windows(&mut windows, main, None, now)?;
    if let Some(extra) = o.get("additional_rate_limits").filter(|v| !v.is_null()) {
        append_additional(&mut windows, extra.as_array()?, now)?;
    }
    Some(windows)
}
fn append_windows(
    windows: &mut Vec<QuotaWindow>,
    rate: &Value,
    additional: Option<&str>,
    now: u64,
) -> Option<()> {
    if !rate.is_null() && !rate.is_object() {
        return None;
    }
    for (key, name) in [
        ("primary_window", "Primary"),
        ("secondary_window", "Secondary"),
    ] {
        let (id, label) = match additional {
            Some(id) => (
                format!("additional:{id}:{key}"),
                format!("Additional quota {} — {name}", &id[..8]),
            ),
            None => (key.into(), name.into()),
        };
        windows.push(window(
            id,
            label,
            rate.get(key).unwrap_or(&Value::Null),
            now,
        )?);
    }
    Some(())
}
fn append_additional(windows: &mut Vec<QuotaWindow>, extra: &[Value], now: u64) -> Option<()> {
    if extra.len() > 8 {
        return None;
    }
    let mut seen = BTreeSet::new();
    for entry in extra {
        let feature = entry.get("metered_feature")?.as_str()?;
        let name = entry.get("limit_name")?.as_str()?;
        let valid_names = valid_text(feature, 128) && valid_text(name, 128);
        if !valid_names || !seen.insert((feature, name)) {
            return None;
        }
        use sha2::Digest as _;
        let id = hex::encode(sha2::Sha256::digest(
            format!("{}:{}:{}", feature.len(), feature, name).as_bytes(),
        ));
        let rate = entry.get("rate_limit").unwrap_or(&Value::Null);
        append_windows(windows, rate, Some(&id), now)?;
    }
    Some(())
}
