//! Owner profile editing: versioned artifacts and project references move atomically.
pub mod inspection;
mod store;
use aq_profiles::{ActivationState, Intelligence, ModelCandidate, ValidatedCatalog};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
pub use store::{operation, read, registry, save, store_artifact};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("database")]
    Database(#[from] sqlx::Error),
    #[error("internal")]
    Internal,
}
pub type Result<T> = std::result::Result<T, Error>;
pub fn profile_error(error: aq_profiles::Error) -> Error {
    match error {
        aq_profiles::Error::RevisionConflict => conflict("catalog revision changed"),
        aq_profiles::Error::IdentityConflict(_) => conflict("immutable profile identity changed"),
        _ => invalid("invalid profile catalog artifact"),
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChoice {
    pub provider: String,
    pub model: String,
    pub reasoning_configuration_reference: String,
    pub reasoning: String,
    pub harness: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRequest {
    pub operation_id: Uuid,
    pub expected_revision: String,
    pub profile_id: String,
    pub profile_version: u32,
    pub name: String,
    pub description: String,
    pub candidates: Vec<ModelCandidate>,
}
fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
fn conflict(message: &str) -> Error {
    Error::Conflict(message.into())
}
fn value<T: Serialize>(v: &T) -> Result<Value> {
    serde_json::to_value(v).map_err(|_| Error::Internal)
}
fn version(state: &ActivationState, namespace: &str, id: &str, current: u32) -> Result<u32> {
    let prefix = format!("{namespace}:{id}@");
    state
        .known_identities
        .keys()
        .filter_map(|key| key.strip_prefix(&prefix)?.parse::<u32>().ok())
        .chain(std::iter::once(current))
        .max()
        .unwrap_or(current)
        .checked_add(1)
        .ok_or_else(|| conflict("Profile version limit reached"))
}
pub struct Prepared {
    pub catalog: ValidatedCatalog,
    pub profile: Intelligence,
    pub references: BTreeMap<String, String>,
    pub changed: bool,
}
pub fn prepare(
    current: &ValidatedCatalog,
    state: &ActivationState,
    req: &SaveRequest,
    choices: &[ModelChoice],
) -> Result<Prepared> {
    if req.operation_id.is_nil()
        || req.name.trim().is_empty()
        || req.name.len() > 128
        || req.description.len() > 2000
        || req.candidates.len() > 16
    {
        return Err(invalid(
            "Enter a profile name and at most sixteen model choices",
        ));
    }
    let creating = req.profile_version == 0;
    let initial = Intelligence {
        id: req.profile_id.clone(),
        version: 0,
        name: None,
        description: None,
        candidates: Vec::new(),
        selection_policy: None,
    };
    let old = if creating {
        let prefix = format!("intelligence:{}@", req.profile_id);
        if current
            .artifact()
            .catalog
            .intelligence
            .iter()
            .any(|p| p.id == req.profile_id)
            || state
                .known_identities
                .keys()
                .any(|key| key.starts_with(&prefix))
        {
            return Err(conflict("A profile with this identity already exists"));
        }
        &initial
    } else {
        current
            .artifact()
            .catalog
            .intelligence
            .iter()
            .find(|p| p.id == req.profile_id && p.version == req.profile_version)
            .ok_or_else(|| conflict("This profile changed. Reload this row before saving"))?
    };
    // Keeping an existing provider mapping does not require that provider to
    // be installed now. Newly chosen execution settings still need known facts.
    for candidate in &req.candidates {
        if old
            .candidates
            .iter()
            .any(|c| value(c).ok() == value(candidate).ok())
        {
            continue;
        }
        if candidate.harnesses.is_empty()
            || !candidate.harnesses.iter().all(|h| {
                choices.iter().any(|c| {
                    c.harness == *h
                        && c.provider == candidate.provider
                        && c.model == candidate.model
                        && c.reasoning_configuration_reference
                            == candidate.reasoning_configuration_reference
                })
            })
        {
            return Err(invalid(
                "This model and reasoning choice is not configured for agent launches",
            ));
        }
    }
    let mut profile = old.clone();
    profile.name = if !creating && old.name.is_none() && req.name.trim() == old.id {
        None
    } else {
        Some(req.name.trim().into())
    };
    profile.description =
        (!req.description.trim().is_empty()).then(|| req.description.trim().into());
    profile.candidates = if req.candidates.is_empty() {
        // An empty spreadsheet row is explicitly unconfigured, not runnable.
        vec![ModelCandidate {
            id: "configuration-required".into(),
            provider: "configuration-required".into(),
            model: "configuration-required".into(),
            reasoning_configuration_reference: "configuration-required:1".into(),
            harnesses: vec!["configuration-required".into()],
        }]
    } else {
        req.candidates.clone()
    };
    if !creating && value(&profile)? == value(old)? {
        return Ok(Prepared {
            catalog: current.clone(),
            profile,
            references: BTreeMap::new(),
            changed: false,
        });
    }
    profile.version = version(state, "intelligence", &profile.id, profile.version)?;
    let old_key = aq_profiles::key(&old.id, old.version);
    let new_key = aq_profiles::key(&profile.id, profile.version);
    let mut catalog = current.artifact().catalog.clone();
    let mut references = BTreeMap::new();
    if creating {
        catalog.intelligence.push(profile.clone());
    } else {
        *catalog
            .intelligence
            .iter_mut()
            .find(|p| p.id == old.id && p.version == old.version)
            .ok_or(Error::Internal)? = profile.clone();
        references.insert(old_key.clone(), new_key.clone());
        for agent in &mut catalog.agents {
            if agent.intelligence != old_key {
                continue;
            }
            let old_agent = aq_profiles::key(&agent.id, agent.version);
            agent.version = version(state, "agent", &agent.id, agent.version)?;
            agent.intelligence = new_key.clone();
            references.insert(old_agent, aq_profiles::key(&agent.id, agent.version));
        }
    }
    let catalog = aq_profiles::validate(catalog, current.artifact().instructions.clone())
        .map_err(|_| invalid("The profile row is invalid. Check its name and model choices"))?;
    Ok(Prepared {
        catalog,
        profile,
        references,
        changed: true,
    })
}
pub fn update_binding(
    binding: &Value,
    digest: &str,
    references: &BTreeMap<String, String>,
) -> Result<Value> {
    let mut out = binding.clone();
    out["catalog_digest"] = json!(digest);
    for field in ["allowed_agent_profiles", "allowed_intelligence_profiles"] {
        let entries = out
            .get_mut(field)
            .and_then(Value::as_array_mut)
            .ok_or(Error::Internal)?;
        for entry in entries {
            if let Some(replacement) = entry.as_str().and_then(|key| references.get(key)) {
                *entry = json!(replacement);
            }
        }
    }
    Ok(out)
}
