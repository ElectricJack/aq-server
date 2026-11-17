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

/// Why the stored choice `profile` (a level id and one of its candidates)
/// cannot run on `snapshot`, or the digest of the snapshot it resolves
/// against. A stored choice names no snapshot: it is resolved against the
/// active one each time it is read or started, so it runs again as soon as
/// the level lists the candidate again.
pub fn stored_choice(
    snapshot: Option<&aq_profiles::roles::Snapshot>,
    profile: &Value,
) -> std::result::Result<String, String> {
    let text = |key: &str| profile[key].as_str().unwrap_or_default();
    let (level, model, harness) = (text("profile_id"), text("model"), text("harness"));
    let Some(found) = snapshot.and_then(|s| s.intelligence(level)) else {
        return Err(format!(
            "its level `{level}` no longer exists; choose a level"
        ));
    };
    let listed = found.candidates.iter().any(|c| {
        c.provider == text("provider")
            && c.model == model
            && c.reasoning == text("reasoning")
            && c.harnesses.iter().any(|h| h == harness)
    });
    if !listed {
        return Err(format!(
            "its level `{level}` no longer lists {model} on {harness}; choose a level"
        ));
    }
    Ok(snapshot.map(|s| s.digest().to_string()).unwrap_or_default())
}
/// [`stored_choice`] against the active snapshot.
pub async fn resolve_stored(
    pool: &PgPool,
    profile: &Value,
) -> Result<std::result::Result<String, String>> {
    let active = aq_role_settings::active_snapshot(pool)
        .await
        .map_err(|error| match error {
            aq_role_settings::Error::Database(error) => Error::from(error),
            _ => bad("The stored roles and profiles are invalid"),
        })?;
    Ok(stored_choice(active.as_ref().map(|a| &a.snapshot), profile))
}

#[cfg(test)]
mod tests {
    use super::stored_choice;
    use aq_profiles::roles::{DocumentLimits, SelectionDefaults, compile, default_documents};
    use serde_json::json;

    #[test]
    fn a_stored_choice_resolves_against_the_snapshot_or_says_why_not() {
        let snapshot = compile(
            &default_documents(),
            &SelectionDefaults::default(),
            &DocumentLimits::default(),
        )
        .unwrap();
        let level = snapshot.intelligence("standard-medium").unwrap();
        let c = &level.candidates[0];
        let choice = json!({"profile_id":"standard-medium","provider":c.provider,"model":c.model,"reasoning":c.reasoning,"harness":c.harnesses[0]});
        assert_eq!(
            stored_choice(Some(&snapshot), &choice),
            Ok(snapshot.digest().to_string())
        );
        let mut other = choice.clone();
        other["model"] = json!("gpt-6.1-sol");
        other["harness"] = json!("codex");
        assert_eq!(
            stored_choice(Some(&snapshot), &other),
            Err(
                "its level `standard-medium` no longer lists gpt-6.1-sol on codex; choose a level"
                    .into()
            )
        );
        other["profile_id"] = json!("fast-low");
        assert_eq!(
            stored_choice(Some(&snapshot), &other),
            Err("its level `fast-low` no longer exists; choose a level".into())
        );
        assert!(stored_choice(None, &choice).is_err());
    }
}
