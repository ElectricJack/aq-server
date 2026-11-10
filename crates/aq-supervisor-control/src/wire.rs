use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Conversation {
    pub id: Uuid,
    pub revision: String,
    pub label: String,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub state: String,
    pub binding: Option<Value>,
    pub reason: String,
}
impl Runtime {
    pub fn live(binding: Option<Value>, ready: bool) -> Self {
        if binding.is_some() {
            Self {
                state: "available".into(),
                binding,
                reason: "central_session_active".into(),
            }
        } else if ready {
            Self {
                state: "idle".into(),
                binding: None,
                reason: "central_service_ready".into(),
            }
        } else {
            Self::unavailable()
        }
    }
    pub fn unavailable() -> Self {
        Self {
            state: "unavailable".into(),
            binding: None,
            reason: "central_runtime_not_connected".into(),
        }
    }
}
#[derive(Serialize)]
pub struct Limits {
    pub body_bytes: u32,
    pub context_refs: u8,
    pub attachments: u8,
    pub history_page: u8,
}
#[derive(Serialize)]
pub struct Discovery {
    pub contract: &'static str,
    pub conversation: Conversation,
    pub runtime: Runtime,
    pub limits: Limits,
}

pub fn recipient(conversation: Uuid) -> Value {
    serde_json::json!({"kind":"global_supervisor","conversation_id":conversation})
}
pub fn author(id: Uuid) -> Value {
    serde_json::json!({"kind":"owner","user_id":id})
}
