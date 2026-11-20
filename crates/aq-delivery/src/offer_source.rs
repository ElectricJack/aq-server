//! First-offer source reads and export identity.
use crate::{DeliveryError, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

const MAX_FRAME: usize = 262_144;

/// Read a fixed public source observation through the central aq-git socket.
pub async fn read(
    socket: &Path,
    repository: Uuid,
    generation: u64,
    operation: &str,
    params: Value,
) -> Result<Value> {
    let id = format!("profiled-source-{}", Uuid::new_v4());
    let request = json!({"protocol_version":1,"request_id":id,"operation":operation,"repository_id":repository,"expected_binding_generation":generation,"params":params,"deadline_ms":5000});
    let exchange = async {
        let mut socket = tokio::net::UnixStream::connect(socket).await?;
        let mut request = serde_json::to_vec(&request)?;
        request.push(b'\n');
        socket.write_all(&request).await?;
        let mut frame = Vec::new();
        BufReader::new(socket)
            .take((MAX_FRAME + 1) as u64)
            .read_until(b'\n', &mut frame)
            .await?;
        if frame.len() > MAX_FRAME || frame.last() != Some(&b'\n') {
            return Err(DeliveryError::Protocol);
        }
        let value: Value = serde_json::from_slice(&frame).map_err(|_| DeliveryError::Protocol)?;
        if value["protocol_version"] != 1
            || value["request_id"] != id
            || value["ok"] != true
            || value.get("error").is_some()
        {
            return Err(DeliveryError::Protocol);
        }
        value.get("result").cloned().ok_or(DeliveryError::Protocol)
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .map_err(|_| DeliveryError::Protocol)?
}

/// Keep the original export key for immutable policies. A followed ref adds
/// the exact commit retained for this offer, so moving the ref gets a new key.
pub fn export_operation(
    project: Uuid,
    task: &impl Serialize,
    project_revision: u64,
    generation: u64,
    policy_ref: impl Serialize,
    commit: Option<&str>,
) -> Result<Uuid> {
    let mut key = match commit {
        None => serde_json::to_value((project, task, project_revision, generation, policy_ref))?,
        Some(commit) => serde_json::to_value((
            project,
            task,
            project_revision,
            generation,
            policy_ref,
            commit,
        ))?,
    };
    key.sort_all_objects();
    Ok(Uuid::new_v5(
        &Uuid::from_bytes(*b"aq-native-export"),
        &serde_json::to_vec(&key)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_a_named_ref_changes_the_export_key_and_immutable_keys_stay_compatible() {
        let project = Uuid::from_u128(7);
        let task = json!({"workspace_id":Uuid::from_u128(8),"task_id":Uuid::from_u128(9),"version":1,"content_revision":1});
        let policy_ref = "policy";
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        let key = |commit| export_operation(project, &task, 3, 1, policy_ref, commit).unwrap();
        assert_ne!(key(Some(&a)), key(Some(&b)));
        assert_eq!(key(Some(&a)), key(Some(&a)));
        assert_ne!(key(Some(&a)), key(None));
        let mut historical = json!((project, &task, 3_u64, 1_u64, policy_ref));
        historical.sort_all_objects();
        assert_eq!(
            key(None),
            Uuid::new_v5(
                &Uuid::from_bytes(*b"aq-native-export"),
                &serde_json::to_vec(&historical).unwrap()
            )
        );
    }
}
