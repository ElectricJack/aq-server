use crate::{Result, cli::CreateTask, error};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap, fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path,
};
use uuid::Uuid;

#[derive(Serialize)]
pub struct Subject {
    #[serde(rename = "type")]
    entity_type: String,
    id: Uuid,
    revision: String,
}
#[derive(Serialize)]
pub struct ActionRequest {
    pub operation_id: Uuid,
    kind: &'static str,
    subject: Subject,
    expected_versions: BTreeMap<String, String>,
    payload: Value,
}
fn action(
    kind: &'static str,
    entity: &str,
    id: Uuid,
    revision: String,
    operation_id: Uuid,
    payload: Value,
) -> ActionRequest {
    ActionRequest {
        operation_id,
        kind,
        subject: Subject {
            entity_type: entity.into(),
            id,
            revision: revision.clone(),
        },
        expected_versions: BTreeMap::from([(entity.into(), revision)]),
        payload,
    }
}
pub fn task(args: &CreateTask) -> ActionRequest {
    let id = Uuid::new_v4();
    action(
        "task.create",
        "project",
        args.project,
        args.expected_project_revision.clone(),
        id,
        json!({
            "operation_id":id,"expected_project_revision":args.expected_project_revision,"title":args.title,"body":args.body,"acceptance_criteria":args.acceptance_criteria,"labels":args.labels,"task_state":args.task_state
        }),
    )
}
pub fn message(conversation: Uuid, revision: String, body: &str) -> ActionRequest {
    let id = Uuid::new_v4();
    action(
        "supervisor.message.send",
        "conversation",
        conversation,
        revision.clone(),
        id,
        json!({
            "operation_id":id,"message_id":Uuid::new_v4(),"expected_conversation_revision":revision,"body":body,"context":[],"attachments":[]
        }),
    )
}
// Request files are bounded regular files; links and pipes are not opened.
fn payload(path: &Path) -> Result<Value> {
    let bad = || {
        error(
            "invalid_request_file",
            "Expected a regular JSON payload file of at most 64 KiB",
        )
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| bad())?;
    if !file.metadata().map_err(|_| bad())?.is_file() {
        return Err(bad());
    }
    let mut bytes = Vec::new();
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| bad())?;
    if bytes.len() > 65536 {
        return Err(bad());
    }
    serde_json::from_slice::<crate::json::UniqueJson>(&bytes)
        .map(|value| value.0)
        .map_err(|_| bad())
}
pub fn decision(id: Uuid, path: &Path) -> Result<ActionRequest> {
    let mut payload = payload(path)?;
    let revision = payload["expected_decision_revision"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            error(
                "invalid_request_file",
                "Expected decision.resolve payload with positive revision",
            )
        })?;
    if payload["decision_id"].as_str() != Some(id.to_string().as_str()) {
        return Err(error(
            "invalid_request_file",
            "Payload decision_id must match the command's decision ID",
        ));
    }
    let operation_id = Uuid::new_v4();
    payload["operation_id"] = json!(operation_id);
    Ok(action(
        "decision.resolve",
        "decision",
        id,
        revision.to_string(),
        operation_id,
        payload,
    ))
}
