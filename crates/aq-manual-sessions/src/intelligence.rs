//! The owner's choices come from the level documents of the active role
//! snapshot (spec 0014 §5.2), never from browser launch settings.
use super::*;
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    /// The level's id.
    pub profile_id: String,
    /// The candidate's derived id: `provider:model:reasoning`.
    pub candidate_id: String,
}
/// The harness a logged-in session runs a provider's models on.
fn login_harness(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("codex"),
        "anthropic" => Some("claude"),
        _ => None,
    }
}
/// One entry per level and candidate that a logged-in harness can run, in
/// level and candidate order, each with the reasoning its document states and
/// the digest of the snapshot it was read from.
pub async fn choices(pool: &PgPool) -> Result<Vec<Value>> {
    let active = aq_role_settings::active_snapshot(pool)
        .await
        .map_err(|error| match error {
            aq_role_settings::Error::Database(error) => Error::from(error),
            _ => bad("The stored roles and profiles are invalid"),
        })?;
    let Some(active) = active else {
        return Ok(Vec::new());
    };
    let snapshot = &active.snapshot;
    let mut out = Vec::new();
    for level in snapshot.intelligence_profiles() {
        for candidate in &level.candidates {
            let Some(harness) = login_harness(&candidate.provider) else {
                continue;
            };
            if !candidate.harnesses.iter().any(|h| h == harness) {
                continue;
            }
            let name = if level.name.trim().is_empty() {
                &level.id
            } else {
                &level.name
            };
            out.push(json!({"snapshot_digest":snapshot.digest(),"profile_id":level.id,"candidate_id":candidate.derived_id(),"name":name,"provider":candidate.provider,"model":candidate.model,"reasoning":candidate.reasoning,"harness":harness}));
        }
    }
    Ok(out)
}
pub(super) async fn resolve(s: &Service, selected: &Selection) -> Result<Value> {
    if selected.profile_id.len() > 96 || selected.candidate_id.len() > 512 {
        return Err(bad("Invalid intelligence profile choice"));
    }
    choices(&s.pool)
        .await?
        .into_iter()
        .find(|c| {
            c["profile_id"] == selected.profile_id && c["candidate_id"] == selected.candidate_id
        })
        .ok_or_else(|| {
            conflict("This intelligence profile changed or is unavailable; select it again")
        })
}
/// The first choice of the level `id`: what a launch without a selection
/// runs, on the project's default level.
pub(super) async fn first_of(s: &Service, id: &str) -> Result<Value> {
    let found = choices(&s.pool).await?;
    found
        .into_iter()
        .find(|c| c["profile_id"] == id)
        .ok_or_else(|| conflict("No installed interactive provider is configured for this project"))
}
