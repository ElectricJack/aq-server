//! Central read provenance is not worker startup content or readiness authority.
use super::{canonical, refused};
use crate::{
    agent_access::scheduling::{ReadyPage, TaskContext},
    error::AppError,
};
use aq_admission::{Digest, TaskRevision, startup::NativeOffer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadProvenance {
    pub schema_version: u32,
    pub task: TaskRevision,
    pub ready_response_digest: Digest,
    pub ready_requested_at_ms: u64,
    pub ready_received_at_ms: u64,
    pub ready_expires_at_ms: u64,
    pub context_response_digest: Digest,
    pub context_requested_at_ms: u64,
    pub context_received_at_ms: u64,
    pub context_expires_at_ms: u64,
    pub authorized_context_digest: Digest,
}
impl ReadProvenance {
    pub fn capture(
        task: &TaskRevision,
        ready: &ReadyPage,
        context: &TaskContext,
        authorized: Digest,
        now: u64,
    ) -> Result<Self, AppError> {
        let selected = ready
            .items
            .iter()
            .find(|v| &v.revision == task)
            .ok_or_else(refused)?;
        if &context.revision != task
            || ready.workspace_id != task.workspace_id
            || canonical(&selected.node)? != canonical(&context.context["node"])?
            || context.stamp.response_digest != Digest::of(context.raw_json.as_bytes()).as_str()
            || context.digest != context.stamp.response_digest
            || context.truncated
            || serde_json::from_str::<Value>(&context.raw_json).map_err(|_| refused())?
                != context.context
        {
            return Err(refused());
        }
        let result = Self {
            schema_version: 1,
            task: task.clone(),
            ready_response_digest: serde_json::from_value(serde_json::json!(
                ready.stamp.response_digest
            ))
            .map_err(|_| refused())?,
            ready_requested_at_ms: ready.stamp.requested_at_ms,
            ready_received_at_ms: ready.stamp.received_at_ms,
            ready_expires_at_ms: ready.stamp.expires_at_ms,
            context_response_digest: serde_json::from_value(serde_json::json!(
                context.stamp.response_digest
            ))
            .map_err(|_| refused())?,
            context_requested_at_ms: context.stamp.requested_at_ms,
            context_received_at_ms: context.stamp.received_at_ms,
            context_expires_at_ms: context.stamp.expires_at_ms,
            authorized_context_digest: authorized,
        };
        result.validate_at(now)?;
        Ok(result)
    }
    pub fn validate_at(&self, now: u64) -> Result<(), AppError> {
        if self.schema_version != 1 || now == 0 {
            return Err(refused());
        }
        self.task.validate().map_err(|_| refused())?;
        for (requested, received, expires) in [
            (
                self.ready_requested_at_ms,
                self.ready_received_at_ms,
                self.ready_expires_at_ms,
            ),
            (
                self.context_requested_at_ms,
                self.context_received_at_ms,
                self.context_expires_at_ms,
            ),
        ] {
            if requested == 0
                || requested > received
                || received > now
                || now >= expires
                || expires.checked_sub(received).is_none_or(|d| d > 30_000)
            {
                return Err(refused());
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredOffer {
    pub schema_version: u32,
    pub read_provenance: ReadProvenance,
    pub native_offer: NativeOffer,
}
impl StoredOffer {
    // jsonb::text adds whitespace. Twice compact bytes plus a fixed allowance is
    // conservative for this finite string/key/array shape under the SQL1MiB cap.
    pub fn value(&self) -> Result<Value, AppError> {
        self.native_offer.validate().map_err(|_| refused())?;
        if self.schema_version != 1 || canonical(self)?.len() > 500_000 {
            return Err(refused());
        }
        serde_json::to_value(self).map_err(|_| AppError::Internal)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_access::scheduling::{GraphReadStamp, ReadyTask};
    use serde_json::json;
    use uuid::Uuid;
    fn reads() -> (TaskRevision, ReadyPage, TaskContext) {
        let task = TaskRevision {
            workspace_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            version: 4,
            content_revision: 7,
        };
        let node = json!({"id":task.task_id,"workspace_id":task.workspace_id,"kind":"task","version":4,"content_revision":7,"body":"authorized task"});
        let context = json!({"node":node,"nodes":[node],"edges":[],"truncated":false});
        let raw_json = String::from_utf8(canonical(&context).unwrap()).unwrap();
        let stamp = GraphReadStamp {
            requested_at_ms: 100,
            received_at_ms: 110,
            expires_at_ms: 30110,
            response_digest: Digest::of(raw_json.as_bytes()).as_str().into(),
        };
        (
            task.clone(),
            ReadyPage {
                workspace_id: task.workspace_id,
                items: vec![ReadyTask {
                    revision: task.clone(),
                    node,
                }],
                next_cursor: None,
                stamp: GraphReadStamp {
                    response_digest: Digest::of(b"actual ready response").as_str().into(),
                    ..stamp.clone()
                },
            },
            TaskContext {
                revision: task,
                context,
                raw_json,
                digest: stamp.response_digest.clone(),
                truncated: false,
                stamp,
            },
        )
    }
    #[test]
    fn same_authorized_context_digest_does_not_replace_exact_read_provenance() {
        let (task, ready, context) = reads();
        let authorized = Digest::of(b"same authorized snapshot");
        let original =
            ReadProvenance::capture(&task, &ready, &context, authorized.clone(), 120).unwrap();
        let mut changed = ready.clone();
        changed.items[0].revision.version += 1;
        assert!(
            ReadProvenance::capture(&task, &changed, &context, authorized.clone(), 120).is_err()
        );
        let mut changed = context.clone();
        changed.revision.content_revision += 1;
        assert!(ReadProvenance::capture(&task, &ready, &changed, authorized.clone(), 120).is_err());
        let mut changed = ready.clone();
        changed.items[0].node["body"] = json!("changed content at same CAS");
        assert!(
            ReadProvenance::capture(&task, &changed, &context, authorized.clone(), 120).is_err()
        );
        let mut changed = context.clone();
        changed.context["node"]["body"] = json!("mutated decoded response");
        assert!(ReadProvenance::capture(&task, &ready, &changed, authorized.clone(), 120).is_err());
        let mut changed = context.clone();
        changed.stamp.response_digest = Digest::of(b"different raw response").as_str().into();
        assert!(ReadProvenance::capture(&task, &ready, &changed, authorized, 120).is_err());
        assert!(original.validate_at(30110).is_err());
        assert!(original.validate_at(109).is_err());
    }
    #[test]
    fn each_read_has_its_own_freshness_and_workspace() {
        let (task, mut ready, context) = reads();
        let digest = Digest::of(b"authorized");
        ready.workspace_id = Uuid::new_v4();
        assert!(ReadProvenance::capture(&task, &ready, &context, digest.clone(), 120).is_err());
        ready.workspace_id = task.workspace_id;
        ready.stamp.expires_at_ms = 120;
        assert!(ReadProvenance::capture(&task, &ready, &context, digest.clone(), 120).is_err());
        ready.stamp.expires_at_ms = 30111;
        assert!(ReadProvenance::capture(&task, &ready, &context, digest, 120).is_err());
    }
}
