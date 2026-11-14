//! Deterministic profile inspection, shared by the existing owner endpoints.
use crate::{Error, Result};
use aq_profiles::{Compilation, CompileRequest, ValidatedCatalog};
use serde::{Deserialize, Serialize};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub artifact_json: String,
    pub composition: Option<CompileRequest>,
    pub graph_access_profile_digest: Option<String>,
}
#[derive(Serialize)]
pub struct Inspection {
    catalog_valid: bool,
    catalog_digest: String,
    selection_pending: bool,
    authority_admission: bool,
    compilation: Option<Compilation>,
}
pub fn inspect(request: Request, require_composition: bool) -> Result<Inspection> {
    if request.artifact_json.len() > 2_097_152 {
        return Err(Error::Invalid("catalog artifact exceeds 2 MiB".into()));
    }
    if require_composition && request.composition.is_none() {
        return Err(Error::Invalid(
            "explain requires a composition snapshot".into(),
        ));
    }
    if request.composition.is_some() != request.graph_access_profile_digest.is_some() {
        return Err(Error::Invalid(
            "composition and access-profile digest must be supplied together".into(),
        ));
    }
    if request
        .composition
        .as_ref()
        .is_some_and(|c| serde_json::to_vec(c).map_or(true, |b| b.len() > 2_097_152))
    {
        return Err(Error::Invalid("composition snapshot exceeds 2 MiB".into()));
    }
    let catalog = ValidatedCatalog::from_artifact(request.artifact_json.as_bytes())
        .map_err(|_| Error::Invalid("invalid profile catalog artifact".into()))?;
    let compilation = request
        .composition
        .as_ref()
        .map(|r| {
            aq_profiles::compile(
                &catalog,
                r,
                request.graph_access_profile_digest.as_deref().unwrap(),
            )
        })
        .transpose()
        .map_err(|_| Error::Invalid("profile composition refused".into()))?;
    Ok(Inspection {
        catalog_valid: true,
        catalog_digest: catalog.digest().into(),
        selection_pending: true,
        authority_admission: false,
        compilation,
    })
}

/// Bound encoded inputs before starting CPU-heavy catalog inspection.
pub fn oversized(request: &Request) -> bool {
    request.artifact_json.len() > 2_097_152
        || request
            .composition
            .as_ref()
            .is_some_and(|c| serde_json::to_vec(c).map_or(true, |b| b.len() > 2_097_152))
}
