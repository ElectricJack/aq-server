//! Resolve owner choices from the current saved catalog, never browser launch settings.
use super::*;
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    pub profile_id: String,
    pub profile_version: u32,
    pub candidate_id: String,
}
pub(super) async fn choices(s: &Service) -> Result<Vec<Value>> {
    let bytes: Option<Vec<u8>> = sqlx::query_scalar("SELECT a.artifact FROM profile_catalog_registry r JOIN profile_catalog_artifacts a ON a.digest=r.active_digest WHERE r.singleton")
        .fetch_optional(&s.pool).await?;
    let Some(bytes) = bytes else {
        return Ok(Vec::new());
    };
    let artifact: Value =
        serde_json::from_slice(&bytes).map_err(|_| bad("Profile catalog is invalid"))?;
    let mut out = Vec::new();
    for profile in artifact["catalog"]["intelligence"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if profile["automatic_routing"] == true {
            continue;
        }
        for c in profile["candidates"].as_array().into_iter().flatten() {
            let harness = match c["provider"].as_str() {
                Some("openai") => "codex",
                Some("anthropic") => "claude",
                _ => continue,
            };
            if !c["harnesses"]
                .as_array()
                .is_some_and(|hs| hs.iter().any(|h| h == harness))
            {
                continue;
            }
            let Some(m) = s.choices.iter().find(|m| {
                m.provider == c["provider"]
                    && m.model == c["model"]
                    && m.reasoning_configuration_reference == c["reasoning_configuration_reference"]
                    && m.harness == harness
            }) else {
                continue;
            };
            let supported: &[&str] = if harness == "codex" {
                &[
                    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
                ]
            } else {
                &["default", "off", "low", "medium", "high", "xhigh", "max"]
            };
            if !supported.contains(&m.reasoning.as_str()) {
                continue;
            }
            out.push(json!({"profile_id":profile["id"],"profile_version":profile["version"],"candidate_id":c["id"],"name":profile["name"].as_str().unwrap_or(profile["id"].as_str().unwrap_or("Profile")),"provider":m.provider,"model":m.model,"reasoning":m.reasoning,"reasoning_configuration_reference":m.reasoning_configuration_reference,"options":m.options,"harness":harness}));
        }
    }
    Ok(out)
}
pub(super) async fn resolve(s: &Service, selected: &Selection) -> Result<Value> {
    if selected.profile_id.len() > 96
        || selected.candidate_id.len() > 512
        || selected.profile_version == 0
    {
        return Err(bad("Invalid intelligence profile choice"));
    }
    choices(s)
        .await?
        .into_iter()
        .find(|c| {
            c["profile_id"] == selected.profile_id
                && c["profile_version"] == selected.profile_version
                && c["candidate_id"] == selected.candidate_id
        })
        .ok_or_else(|| {
            conflict("This intelligence profile changed or is unavailable; select it again")
        })
}
