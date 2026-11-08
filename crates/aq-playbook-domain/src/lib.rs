//! Fixed host contracts and typed grant scope. No generic SQL/endpoint/command proxy.
use aq_playbook_host::{
    model::*,
    runner::{Authority, Intent},
};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("playbook_invalid_request")]
    Invalid,
    #[error("playbook_scope_refused")]
    Forbidden,
    #[error("playbook_identity_conflict")]
    Conflict,
    #[error("playbook_capacity_exhausted")]
    Overloaded,
    #[error("playbook_runtime_fenced")]
    Fenced,
    #[error("playbook_storage_unknown")]
    Unknown,
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<sqlx::Error> for Error {
    fn from(_: sqlx::Error) -> Self {
        Self::Unknown
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::Invalid
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Recipient {
    Task {
        workspace_id: Uuid,
        project_id: Uuid,
        task_id: Uuid,
    },
    Session {
        workspace_id: Uuid,
        project_id: Uuid,
        task_id: Uuid,
        session_id: Uuid,
        attempt_id: Uuid,
        generation: u64,
    },
}
impl Recipient {
    pub fn scope(&self) -> (Uuid, Uuid, Uuid) {
        match *self {
            Self::Task {
                workspace_id,
                project_id,
                task_id,
            }
            | Self::Session {
                workspace_id,
                project_id,
                task_id,
                ..
            } => (workspace_id, project_id, task_id),
        }
    }
    pub fn validate(&self) -> Result<()> {
        let (w, p, t) = self.scope();
        if [w, p, t].iter().any(Uuid::is_nil) {
            return Err(Error::Invalid);
        }
        if let Self::Session {
            session_id,
            attempt_id,
            generation,
            ..
        } = self
            && (session_id.is_nil()
                || attempt_id.is_nil()
                || *generation == 0
                || *generation > i64::MAX as u64)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GrantScope {
    pub workspace_id: Uuid,
    pub project_id: Uuid,
    pub recipients: Vec<Recipient>,
    pub capabilities: BTreeSet<String>,
}
impl GrantScope {
    pub fn validate(&self) -> Result<()> {
        if self.workspace_id.is_nil()
            || self.project_id.is_nil()
            || self.recipients.is_empty()
            || self.recipients.len() > 128
            || self.capabilities != BTreeSet::from(["message.send".into()])
        {
            return Err(Error::Invalid);
        }
        let mut seen = BTreeSet::new();
        for r in &self.recipients {
            r.validate()?;
            let (w, p, _) = r.scope();
            if w != self.workspace_id
                || p != self.project_id
                || !seen.insert(serde_json::to_string(r)?)
            {
                return Err(Error::Invalid);
            }
        }
        if canonical_bytes(self).map_err(|_| Error::Invalid)?.len() > 32768 {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    pub fn permits(&self, r: &Recipient) -> bool {
        self.recipients.contains(r)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageInput {
    pub recipient: Recipient,
    pub thread_id: Uuid,
    pub body: String,
}
impl MessageInput {
    pub fn validate(&self) -> Result<()> {
        self.recipient.validate()?;
        if self.thread_id.is_nil() || self.body.is_empty() || self.body.len() > 16384 {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
pub fn authority(grant_id: Uuid, revision: i64) -> Authority {
    Authority {
        actor: format!("aq.playbook.grant/{grant_id}/{revision}"),
        scope: "project".into(),
        capabilities: BTreeSet::from(["message.send".into()]),
    }
}
pub fn recipient_schema() -> Schema {
    let base = || {
        BTreeMap::from([
            ("workspace_id".into(), Schema::string(36)),
            ("project_id".into(), Schema::string(36)),
            ("task_id".into(), Schema::string(36)),
        ])
    };
    let kind = |name: &str| Schema::String {
        max_length: 16,
        values: BTreeSet::from([name.into()]),
    };
    let mut task = base();
    task.insert("kind".into(), kind("task"));
    let mut session = base();
    session.insert("kind".into(), kind("session"));
    session.insert("session_id".into(), Schema::string(36));
    session.insert("attempt_id".into(), Schema::string(36));
    session.insert(
        "generation".into(),
        Schema::Integer {
            minimum: 1,
            maximum: i64::MAX,
        },
    );
    Schema::Union {
        variants: vec![Schema::object(task), Schema::object(session)],
    }
}
pub fn message_contract() -> CommandContract {
    let fixed = |value: &str| Schema::String {
        max_length: 64,
        values: BTreeSet::from([value.into()]),
    };
    let output = Schema::object(BTreeMap::from([
        (
            "api_version".into(),
            Schema::Integer {
                minimum: 1,
                maximum: 1,
            },
        ),
        ("operation_id".into(), Schema::string(36)),
        ("message_id".into(), Schema::string(36)),
        ("recipient".into(), recipient_schema()),
        ("thread_id".into(), Schema::string(36)),
        ("cursor".into(), Schema::string(32)),
        ("disposition".into(), fixed("queued")),
        ("node_delivery".into(), fixed("not_attempted")),
        ("harness_delivery".into(), fixed("not_attempted")),
        ("notification_delivery".into(), fixed("not_attempted")),
    ]));
    CommandContract {
        name: "message.send".into(),
        version: "1".into(),
        input: Schema::object(BTreeMap::from([
            ("recipient".into(), recipient_schema()),
            ("thread_id".into(), Schema::string(36)),
            ("body".into(), Schema::string(16384)),
        ])),
        outcomes: BTreeMap::from([("queued".into(), output)]),
        capabilities: BTreeSet::from(["message.send".into()]),
        scopes: BTreeSet::from(["project".into()]),
        effect: Effect::Mutation,
        recovery: Recovery::Reconcilable,
        asynchronous: None,
        cancellation: true,
        public_result_fields: BTreeSet::from(["message_id".into(), "disposition".into()]),
        sensitive_input_fields: BTreeSet::from(["body".into()]),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 65536,
        deadline_ms: 5000,
    }
}
pub fn operation_event() -> EventContract {
    EventContract {
        name: "aq.operation.updated".into(),
        version: "1".into(),
        payload: Schema::object(BTreeMap::from([
            ("operation_id".into(), Schema::string(36)),
            ("message_id".into(), Schema::string(36)),
            (
                "disposition".into(),
                Schema::String {
                    max_length: 16,
                    values: BTreeSet::from(["queued".into()]),
                },
            ),
        ])),
        trusted_producers: BTreeSet::from(["aq.host.bridge@1".into()]),
        scopes: BTreeSet::from(["project".into()]),
        correlation_fields: BTreeSet::from(["operation".into()]),
        subject_field: Some("task".into()),
        revision_field: None,
        max_payload_bytes: 1024,
        retention_ms: 86400000,
        sensitive_payload_fields: BTreeSet::new(),
    }
}
pub fn registry() -> Registry {
    let c = message_contract();
    let e = operation_event();
    Registry {
        commands: BTreeMap::from([(c.name.clone(), c)]),
        events: BTreeMap::from([(e.name.clone(), e)]),
    }
}
pub fn validate_intent(intent: &Intent, grant_id: Uuid, revision: i64) -> Result<MessageInput> {
    if intent.authority != authority(grant_id, revision) {
        return Err(Error::Forbidden);
    }
    message_input(intent)
}
pub fn message_input(intent: &Intent) -> Result<MessageInput> {
    if intent.command != message_contract() {
        return Err(Error::Forbidden);
    }
    if intent.input_digest != digest(&intent.input)?
        || intent.id.is_empty()
        || intent.id.len() > 512
        || intent.run.is_empty()
        || intent.run.len() > 512
        || intent.deadline_at_ms <= 0
    {
        return Err(Error::Invalid);
    }
    let input: MessageInput = serde_json::from_value(intent.input.clone())?;
    input.validate()?;
    Ok(input)
}
pub fn digest(value: &impl Serialize) -> Result<String> {
    aq_playbook_host::model::digest(value).map_err(|_| Error::Invalid)
}
pub fn stable_id(domain: &str, namespace: Uuid, generation: i64, key: &str) -> Result<Uuid> {
    let bytes =
        canonical_bytes(&(domain, namespace, generation, key)).map_err(|_| Error::Invalid)?;
    let hash = hash_bytes(&bytes);
    let data = hex::decode(hash.strip_prefix("sha256:").ok_or(Error::Invalid)?)
        .map_err(|_| Error::Invalid)?;
    let mut id: [u8; 16] = data[..16].try_into().map_err(|_| Error::Invalid)?;
    id[6] = (id[6] & 15) | 0x80;
    id[8] = (id[8] & 63) | 0x80;
    Ok(Uuid::from_bytes(id))
}

pub fn grant_identity(value: &Authority) -> Result<(Uuid, i64)> {
    let (id, revision) = grant_actor(value)?;
    if value != &authority(id, revision) {
        return Err(Error::Forbidden);
    }
    Ok((id, revision))
}
/// Actor parsing is not authorization. Every profile2 callback separately
/// compares full authority/capabilities with the locked immutable grant.
pub fn grant_actor(value: &Authority) -> Result<(Uuid, i64)> {
    let suffix = value
        .actor
        .strip_prefix("aq.playbook.grant/")
        .ok_or(Error::Forbidden)?;
    let (id, revision) = suffix.split_once('/').ok_or(Error::Forbidden)?;
    let id: Uuid = id.parse().map_err(|_| Error::Forbidden)?;
    let revision: i64 = revision.parse().map_err(|_| Error::Forbidden)?;
    if id.is_nil()
        || revision <= 0
        || value.actor != authority(id, revision).actor
        || value.scope != "project"
    {
        return Err(Error::Forbidden);
    }
    Ok((id, revision))
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let code = self.to_string();
        let unknown = self == Error::Unknown;
        let retryable = self == Error::Overloaded;
        let status = match self {
            Error::Invalid => StatusCode::BAD_REQUEST,
            Error::Forbidden => StatusCode::NOT_FOUND,
            Error::Conflict | Error::Fenced => StatusCode::CONFLICT,
            Error::Overloaded => StatusCode::TOO_MANY_REQUESTS,
            Error::Unknown => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status,Json(json!({"code":code,"disposition":if unknown{"unknown"}else{"refused"},"retryable":retryable}))).into_response()
    }
}
