//! Fixed public Git observations for a configured project source policy.
//! These facts are proposals until retained and fenced with the actual offer.
use super::config::ProjectPolicyRecord;
use crate::{AppState, error::AppError, project_bindings::domain::FrozenProjectBinding};
use aq_admission::{
    ArtifactRef, Digest, ObjectFormat, ObjectId, SourceAdmission, SourceSelection, Text,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::OnceLock, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::Semaphore,
};
use uuid::Uuid;
const MAX_FRAME: usize = 262_144;
fn refused() -> AppError {
    AppError::Conflict("configured source observation unavailable or inconsistent".into())
}
pub struct Observation {
    pub admission: SourceAdmission,
    pub retention_handle: Uuid,
    pub artifact: ArtifactRef,
    pub bytes: Vec<u8>,
}
async fn read(
    state: &AppState,
    repository: Uuid,
    generation: u64,
    operation: &str,
    params: Value,
) -> Result<Value, AppError> {
    let runtime = state.source_retention.as_ref().ok_or_else(refused)?;
    let id = format!("profiled-source-{}", Uuid::new_v4());
    let request = json!({"protocol_version":1,"request_id":id,"operation":operation,"repository_id":repository,"expected_binding_generation":generation,"params":params,"deadline_ms":5000});
    let exchange = async {
        let mut socket = tokio::net::UnixStream::connect(&runtime.config.git_socket)
            .await
            .map_err(|_| refused())?;
        let mut request = serde_json::to_vec(&request).map_err(|_| AppError::Internal)?;
        request.push(b'\n');
        socket.write_all(&request).await.map_err(|_| refused())?;
        let mut frame = Vec::new();
        BufReader::new(socket)
            .take((MAX_FRAME + 1) as u64)
            .read_until(b'\n', &mut frame)
            .await
            .map_err(|_| refused())?;
        if frame.len() > MAX_FRAME || frame.last() != Some(&b'\n') {
            return Err(refused());
        }
        let value: Value = serde_json::from_slice(&frame).map_err(|_| refused())?;
        if value["protocol_version"] != 1
            || value["request_id"] != id
            || value["ok"] != true
            || value.get("error").is_some()
        {
            return Err(refused());
        }
        value.get("result").cloned().ok_or_else(refused)
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .map_err(|_| refused())?
}
/// Initial source mode requires an exact retained SHA1 commit. Annotated tags,
/// SHA256 and retention of an ancestor through a descendant handle refuse here.
/// No implicit refresh/fetch, reference creation or retention mutation occurs.
pub async fn observe(
    state: &AppState,
    project: &FrozenProjectBinding,
    policy: &ProjectPolicyRecord,
) -> Result<Observation, AppError> {
    static READS: OnceLock<Semaphore> = OnceLock::new();
    let _permit = READS
        .get_or_init(|| Semaphore::new(4))
        .try_acquire()
        .map_err(|_| refused())?;
    let configured = state
        .config
        .profiled_execution
        .as_deref()
        .ok_or_else(refused)?
        .project_policy_record(project.project.binding.project_id)
        .map_err(|_| refused())?;
    if !policy.enabled
        || serde_json::to_value(policy).map_err(|_| AppError::Internal)?
            != serde_json::to_value(configured).map_err(|_| AppError::Internal)?
    {
        return Err(refused());
    }
    let binding = &project.project.binding;
    let mut observed = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
    if observed == 0
        || !binding.enabled
        || project.project.revision == 0
        || binding.repository_id.is_nil()
        || binding.repository_binding_generation == 0
        || project.node_id.is_nil()
        || observed < project.binding_observation.observed_at_ms
        || observed >= project.binding_observation.expires_at_ms
    {
        return Err(refused());
    }
    let mut evidence = BTreeMap::new();
    let object = match &policy.source {
        SourceSelection::Immutable { object } => object.clone(),
        SourceSelection::NamedRef { reference } => {
            let response = read(
                state,
                binding.repository_id,
                binding.repository_binding_generation,
                "repo.observation",
                json!({"refs":[reference.as_str()],"max_age_ms":30000}),
            )
            .await?;
            let received =
                u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
            if received < observed {
                return Err(refused());
            }
            let object = resolve_ref(
                &response,
                reference.as_str(),
                binding.repository_binding_generation,
                received,
            )?;
            observed = observed.min(response["started_at_ms"].as_u64().ok_or_else(refused)?);
            evidence.insert("reference_observation", response);
            object
        }
    };
    object.validate().map_err(|_| refused())?;
    if object.format != ObjectFormat::Sha1 {
        return Err(refused());
    }
    let inspected = read(
        state,
        binding.repository_id,
        binding.repository_binding_generation,
        "inspect",
        json!({"oid":{"algorithm":"sha1","hex":object.hex}}),
    )
    .await?;
    if inspected["oid"] != json!({"algorithm":"sha1","hex":object.hex})
        || inspected["tree"]["algorithm"] != "sha1"
        || !hex40(inspected["tree"]["hex"].as_str().unwrap_or(""))
    {
        return Err(refused());
    }
    evidence.insert("commit_inspection", inspected);
    let retained = read(
        state,
        binding.repository_id,
        binding.repository_binding_generation,
        "retention.inspect",
        json!({"handle_id":policy.retention_handle,"limit":1}),
    )
    .await?;
    verify_retention(
        &retained,
        policy.retention_handle,
        binding.repository_id,
        binding.repository_binding_generation,
        &object,
    )?;
    evidence.insert("retention_inspection", retained);
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
    if now < observed || now >= observed.checked_add(30000).ok_or_else(refused)? {
        return Err(refused());
    }
    let admission = SourceAdmission {
        schema_version: 1,
        project_id: binding.project_id,
        project_revision: project.project.revision,
        repository_id: binding.repository_id,
        repository_binding_generation: binding.repository_binding_generation,
        policy: policy.artifact().map_err(|_| refused())?,
        selection: policy.source.clone(),
        resolved_object: object,
        observed_at_ms: observed,
        expires_at_ms: observed + 30000,
    };
    admission.validate().map_err(|_| refused())?;
    let mut body = json!({"schema_version":1,"admission":admission,"retention_handle":policy.retention_handle,"evidence":evidence});
    body.sort_all_objects();
    let bytes = serde_json::to_vec(&body).map_err(|_| AppError::Internal)?;
    if bytes.len() > 1_048_576 {
        return Err(refused());
    }
    let observation_digest = Digest::of(&bytes);
    let artifact = ArtifactRef {
        id: Text::new(format!(
            "aq.source-admission-observation/{}",
            observation_digest.as_str()
        ))
        .map_err(|_| AppError::Internal)?,
        version: Text::new("1").map_err(|_| AppError::Internal)?,
        digest: observation_digest,
    };
    Ok(Observation {
        admission,
        retention_handle: policy.retention_handle,
        artifact,
        bytes,
    })
}
/// Resolve the actual configured repository's logical identity, never a path or
/// guessed UUID alias. The export owner subsequently verifies the exact name.
pub(crate) async fn repository_identity(
    state: &AppState,
    repository: Uuid,
    generation: u64,
) -> Result<String, AppError> {
    let value = read(state, repository, generation, "repo.show", json!({})).await?;
    let identity = value["identity"].as_str().ok_or_else(refused)?;
    if value["repository_id"] != repository.to_string()
        || value["binding_generation"] != generation
        || value["mode"] != "central"
        || value["object_format"] != "sha1"
        || identity.trim().is_empty()
        || identity.len() > 512
        || identity.chars().any(char::is_control)
    {
        return Err(refused());
    }
    Ok(identity.to_owned())
}
fn hex40(s: &str) -> bool {
    s.len() == 40
        && s.bytes().any(|b| b != b'0')
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn resolve_ref(
    value: &Value,
    reference: &str,
    generation: u64,
    now: u64,
) -> Result<ObjectId, AppError> {
    let completed = value["completed_at_ms"].as_u64().ok_or_else(refused)?;
    let started = value["started_at_ms"].as_u64().ok_or_else(refused)?;
    if started == 0
        || completed < started
        || completed > now
        || now - completed > 30000
        || value["binding_generation"] != generation
        || value["observation_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .is_none_or(|id| id.is_nil())
        || value["covered_refs"]
            .as_array()
            .is_none_or(|a| a.len() > 128 || !a.contains(&json!(reference)))
        || value.get("last_refresh_error").is_none_or(|v| !v.is_null())
    {
        return Err(refused());
    }
    let hex = value["refs"][reference].as_str().ok_or_else(refused)?;
    if !hex40(hex) {
        return Err(refused());
    }
    Ok(ObjectId {
        format: ObjectFormat::Sha1,
        hex: hex.into(),
    })
}
fn verify_retention(
    value: &Value,
    handle: Uuid,
    repository: Uuid,
    generation: u64,
    object: &ObjectId,
) -> Result<(), AppError> {
    let refs = value["refs"].as_array().ok_or_else(refused)?;
    let mut names = std::collections::BTreeSet::new();
    if refs.iter().any(|r| {
        r["name"]
            .as_str()
            .is_none_or(|name| name.is_empty() || name.len() > 4096 || !names.insert(name))
            || !hex40(r["oid"].as_str().unwrap_or(""))
    }) {
        return Err(refused());
    }
    if value["version"] != 1
        || value["id"] != handle.to_string()
        || value["repository_id"] != repository.to_string()
        || value["binding_generation"] != generation
        || value["generation"].as_u64().is_none_or(|g| g == 0)
        || value["state"] != "active"
        || refs.len() > 256
        || !refs.iter().any(|r| r["oid"] == object.hex)
        || value.get("release_operation").is_none_or(|v| !v.is_null())
        || value.get("release_reason").is_none_or(|v| !v.is_null())
    {
        return Err(refused());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_observation_requires_original_complete_fresh_exact_generation() {
        let reference = "refs/heads/main";
        let valid = json!({"observation_id":Uuid::new_v4(),"binding_generation":2,"started_at_ms":1000,"completed_at_ms":1100,"covered_refs":[reference],"refs":{reference:"a".repeat(40)},"last_refresh_error":null});
        assert!(resolve_ref(&valid, reference, 2, 1200).is_ok());
        for (field, bad) in [
            ("observation_id", json!(Uuid::nil())),
            ("binding_generation", json!(3)),
            ("started_at_ms", json!(1200)),
            ("completed_at_ms", json!(1300)),
            ("covered_refs", json!([])),
            ("last_refresh_error", json!({"code":"offline"})),
        ] {
            let mut value = valid.clone();
            value[field] = bad;
            assert!(resolve_ref(&value, reference, 2, 1200).is_err(), "{field}");
        }
        assert!(resolve_ref(&valid, reference, 2, 31_101).is_err());
    }
    #[test]
    fn retention_is_exact_active_complete_and_never_inferred_from_missing_fields() {
        let handle = Uuid::new_v4();
        let repository = Uuid::new_v4();
        let object = ObjectId {
            format: ObjectFormat::Sha1,
            hex: "a".repeat(40),
        };
        let valid = json!({"version":1,"id":handle,"repository_id":repository,"binding_generation":2,"generation":1,"state":"active","refs":[{"name":"refs/aq-git/checkpoints/test","oid":object.hex}],"release_operation":null,"release_reason":null});
        assert!(verify_retention(&valid, handle, repository, 2, &object).is_ok());
        for (field, bad) in [
            ("state", json!("released")),
            ("id", json!(Uuid::new_v4())),
            ("repository_id", json!(Uuid::new_v4())),
            ("binding_generation", json!(3)),
            ("generation", json!(0)),
            ("refs", json!([])),
            ("release_operation", json!(Uuid::new_v4())),
        ] {
            let mut value = valid.clone();
            value[field] = bad;
            assert!(
                verify_retention(&value, handle, repository, 2, &object).is_err(),
                "{field}"
            );
        }
        let mut incomplete = valid.clone();
        incomplete
            .as_object_mut()
            .unwrap()
            .remove("release_operation");
        assert!(verify_retention(&incomplete, handle, repository, 2, &object).is_err());
    }
}

mod ownership;
pub use ownership::{retain_admission, validate_body, validate_stored_body};
