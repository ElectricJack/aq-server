use super::{domain::*, *};
use crate::agent_access::WorkspaceVerificationError;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Semaphore;
static READS: OnceLock<Semaphore> = OnceLock::new();
pub(super) async fn observe(state: &AppState, binding: &ProjectBinding) -> BindingObservation {
    observe_with_gate(state, binding, READS.get_or_init(|| Semaphore::new(4))).await
}
pub(super) async fn observe_execution(
    state: &AppState,
    binding: &ProjectBinding,
) -> BindingObservation {
    static EXECUTION_READS: OnceLock<Semaphore> = OnceLock::new();
    observe_with_gate(
        state,
        binding,
        EXECUTION_READS.get_or_init(|| Semaphore::new(2)),
    )
    .await
}
async fn observe_with_gate(
    state: &AppState,
    binding: &ProjectBinding,
    reads: &Semaphore,
) -> BindingObservation {
    let now = (state.clock)().timestamp_millis().max(1) as u64;
    let unavailable = || BindingObservation {
        repository_id: binding.repository_id,
        graph_workspace_id: binding.graph_workspace_id,
        repository_adapter_reference: binding.repository_adapter_reference.clone(),
        graph_adapter_reference: binding.graph_adapter_reference.clone(),
        repository: ExternalStatus::Unavailable,
        graph: ExternalStatus::Unavailable,
        repository_generation: None,
        observed_at_ms: now,
        expires_at_ms: now.saturating_add(30_000),
    };
    let Ok(_permit) = reads.try_acquire() else {
        return unavailable();
    };
    let git = async {
        match &state.source_retention {
            Some(rt) => {
                rt.verify_repository(
                    binding.repository_id,
                    binding.repository_binding_generation,
                    binding.publication.as_ref().map(|p| p.target.as_str()),
                )
                .await
            }
            None => RepositoryVerification::Unavailable,
        }
    };
    let graph = async {
        match &state.agent_access {
            Some(rt) => rt.verify_workspace(binding.graph_workspace_id).await,
            None => Err(WorkspaceVerificationError::Unavailable),
        }
    };
    let (git, graph) = tokio::join!(git, graph);
    let (repository, generation) = match git {
        RepositoryVerification::Verified(g) => (ExternalStatus::Verified, Some(g)),
        RepositoryVerification::Absent => (ExternalStatus::Absent, None),
        RepositoryVerification::Stale(g) => (ExternalStatus::Stale, g),
        RepositoryVerification::Forbidden => (ExternalStatus::Forbidden, None),
        RepositoryVerification::Unsupported => (ExternalStatus::Unsupported, None),
        RepositoryVerification::Unavailable => (ExternalStatus::Unavailable, None),
    };
    let graph = match graph {
        Ok(()) => ExternalStatus::Verified,
        Err(WorkspaceVerificationError::NotFound) => ExternalStatus::Absent,
        Err(WorkspaceVerificationError::Forbidden) => ExternalStatus::Forbidden,
        Err(WorkspaceVerificationError::Unavailable) => ExternalStatus::Unavailable,
    };
    // The observation age starts before either request, never after slow I/O.
    BindingObservation {
        repository_id: binding.repository_id,
        graph_workspace_id: binding.graph_workspace_id,
        repository_adapter_reference: binding.repository_adapter_reference.clone(),
        graph_adapter_reference: binding.graph_adapter_reference.clone(),
        repository,
        graph,
        repository_generation: generation,
        observed_at_ms: now,
        expires_at_ms: now.saturating_add(30_000),
    }
}

/// Narrow server-configured aq-git v1 read for project policy. No caller path,
/// URL, credential, arbitrary operation or private Git state is accepted.
#[derive(Debug)]
pub(crate) enum RepositoryVerification {
    Verified(u64),
    Absent,
    Stale(Option<u64>),
    Forbidden,
    Unavailable,
    Unsupported,
}
#[derive(Deserialize)]
struct VerificationEnvelope {
    protocol_version: u32,
    request_id: String,
    ok: bool,
    result: Option<VerificationRepository>,
    #[serde(default, deserialize_with = "present_value")]
    error: Option<Value>,
}
#[derive(Deserialize)]
struct VerificationRepository {
    repository_id: Uuid,
    binding_generation: u64,
    mode: String,
    object_format: String,
    /// aq-git binding allowlist; a publication policy may only name one of these.
    #[serde(default)]
    allowed_targets: Vec<String>,
}
/// Exact spelling: aq-git authorizes the literal branch, so case-folding here
/// would admit a policy aq-git later refuses with `unsupported_feature`.
fn target_allowed(allowed: &[String], target: Option<&str>) -> bool {
    target.is_none_or(|t| allowed.iter().any(|a| a == t))
}
fn present_value<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Ok(Some(Value::deserialize(d)?))
}
impl crate::source_retention::Runtime {
    pub(crate) async fn verify_repository(
        &self,
        id: Uuid,
        generation: u64,
        publication_target: Option<&str>,
    ) -> RepositoryVerification {
        let request_id = format!("project-repo-{}", Uuid::new_v4());
        let request = json!({"protocol_version":1,"request_id":request_id,"operation":"repo.show","repository_id":id,"expected_binding_generation":generation,"params":{},"deadline_ms":5000});
        let exchange = async {
            let mut socket = tokio::net::UnixStream::connect(&self.config.git_socket).await?;
            let mut bytes = serde_json::to_vec(&request)?;
            bytes.push(b'\n');
            socket.write_all(&bytes).await?;
            let mut frame = Vec::new();
            let mut reader = BufReader::new(socket).take(65_537);
            reader.read_until(b'\n', &mut frame).await?;
            anyhow::ensure!(
                frame.len() <= 65_536 && frame.last() == Some(&b'\n'),
                "invalid project repository frame"
            );
            let value: VerificationEnvelope = serde_json::from_slice(&frame)?;
            anyhow::ensure!(
                value.protocol_version == 1 && value.request_id == request_id,
                "repository response mismatch"
            );
            Ok::<_, anyhow::Error>(value)
        };
        let value = match tokio::time::timeout(Duration::from_secs(5), exchange).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                return if e
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
                {
                    RepositoryVerification::Forbidden
                } else {
                    RepositoryVerification::Unavailable
                };
            }
            Err(_) => return RepositoryVerification::Unavailable,
        };
        if !value.ok && value.result.is_none() {
            let error = value.error.unwrap_or(Value::Null);
            return match error["code"].as_str() {
                Some("repository_not_found") => RepositoryVerification::Absent,
                Some("binding_conflict") => {
                    RepositoryVerification::Stale(error["details"]["binding_generation"].as_u64())
                }
                Some("forbidden" | "unauthorized") => RepositoryVerification::Forbidden,
                Some("unsupported_protocol" | "unsupported_feature") => {
                    RepositoryVerification::Unsupported
                }
                _ => RepositoryVerification::Unavailable,
            };
        }
        if !value.ok || value.error.is_some() {
            return RepositoryVerification::Unavailable;
        }
        let Some(result) = value.result else {
            return RepositoryVerification::Unavailable;
        };
        if result.repository_id != id {
            return RepositoryVerification::Unavailable;
        }
        if result.mode != "central" || result.object_format != "sha1" {
            return RepositoryVerification::Unsupported;
        }
        if !target_allowed(&result.allowed_targets, publication_target) {
            return RepositoryVerification::Forbidden;
        }
        match result.binding_generation {
            g if g == generation && g > 0 => RepositoryVerification::Verified(g),
            g if g > 0 => RepositoryVerification::Stale(Some(g)),
            _ => RepositoryVerification::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_target_must_be_an_allowed_target() {
        let allowed = vec!["refs/heads/main".to_string()];
        assert!(target_allowed(&allowed, None));
        assert!(target_allowed(&[], None));
        assert!(target_allowed(&allowed, Some("refs/heads/main")));
        assert!(!target_allowed(&allowed, Some("refs/heads/Main")));
        assert!(!target_allowed(&allowed, Some("refs/heads/release")));
        assert!(!target_allowed(&[], Some("refs/heads/main")));
    }
    #[test]
    fn repository_reply_rejects_duplicate_subject_and_envelope_fields() {
        let payload = r#"{"protocol_version":1,"request_id":"read","ok":true,"result":{"repository_id":"00000000-0000-0000-0000-000000000002","binding_generation":1,"mode":"central","object_format":"sha1"}}"#;
        assert!(serde_json::from_str::<VerificationEnvelope>(payload).is_ok());
        assert!(
            serde_json::from_str::<VerificationEnvelope>(&payload.replace(
                "\"binding_generation\":1",
                "\"binding_generation\":1,\"binding_generation\":2"
            ))
            .is_err()
        );
        assert!(
            serde_json::from_str::<VerificationEnvelope>(
                &payload.replace("\"ok\":true", "\"ok\":false,\"ok\":true")
            )
            .is_err()
        );
    }
}
