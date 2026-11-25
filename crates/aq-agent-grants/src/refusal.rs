//! The structured refusal (§6.8). It describes only the caller's own
//! authority: the command, the grant it lacked and the grants of the same
//! family it holds. It never names or confirms any other record.
use super::{Grant, RequiredGrant};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// The caller holds no grant for this command.
    GrantMissing,
    /// The caller holds the grant, but the target is outside its scope.
    ScopeDenied,
    /// The command can never be granted to an agent.
    OwnerOnly,
    /// A grant limit such as `max_created_tasks` was reached.
    QuotaExceeded,
    SessionRevoked,
    SessionExpired,
    /// The channel proxy refused the path (aq-worker).
    ChannelRefused,
    /// The command surface's own errors: no such command, no route for this
    /// principal, a malformed request, no credential, or a server failure.
    UnknownCommand,
    NotAvailable,
    InvalidRequest,
    Unauthorized,
    Internal,
    /// The attempt's command budget is spent for now; retry later. Also the
    /// server's agent access is not configured or is unavailable.
    Overloaded,
    /// The request names an agent API version this server does not speak.
    UnsupportedApiVersion,
}
/// Every code, in the published order.
pub const CODES: [RefusalCode; 14] = [
    RefusalCode::GrantMissing,
    RefusalCode::ScopeDenied,
    RefusalCode::OwnerOnly,
    RefusalCode::QuotaExceeded,
    RefusalCode::SessionRevoked,
    RefusalCode::SessionExpired,
    RefusalCode::ChannelRefused,
    RefusalCode::UnknownCommand,
    RefusalCode::NotAvailable,
    RefusalCode::InvalidRequest,
    RefusalCode::Unauthorized,
    RefusalCode::Internal,
    RefusalCode::Overloaded,
    RefusalCode::UnsupportedApiVersion,
];
impl RefusalCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GrantMissing => "grant_missing",
            Self::ScopeDenied => "scope_denied",
            Self::OwnerOnly => "owner_only",
            Self::QuotaExceeded => "quota_exceeded",
            Self::SessionRevoked => "session_revoked",
            Self::SessionExpired => "session_expired",
            Self::ChannelRefused => "channel_refused",
            Self::UnknownCommand => "unknown_command",
            Self::NotAvailable => "not_available",
            Self::InvalidRequest => "invalid_request",
            Self::Unauthorized => "unauthorized",
            Self::Internal => "internal",
            Self::Overloaded => "overloaded",
            Self::UnsupportedApiVersion => "unsupported_api_version",
        }
    }
    /// The closed agent-access-1 `ErrorResponse` code that carries this
    /// refusal on the `/v1/agent/*` routes for clients that did not opt in
    /// to the structured shape.
    pub fn legacy(self) -> &'static str {
        match self {
            Self::GrantMissing | Self::OwnerOnly | Self::NotAvailable => "unsupported_capability",
            Self::UnknownCommand | Self::InvalidRequest => "invalid_request",
            Self::Unauthorized => "unauthorized",
            Self::Internal => "internal",
            Self::Overloaded => "overloaded",
            Self::ScopeDenied | Self::QuotaExceeded | Self::ChannelRefused => "scope_denied",
            Self::SessionRevoked => "session_revoked",
            Self::SessionExpired => "session_expired",
            Self::UnsupportedApiVersion => "unsupported_api_version",
        }
    }
    /// The refusal for an agent-access authentication error code. Only a
    /// rejected credential is `unauthorized` (401), so nothing else can
    /// provoke the worker's credential re-issue.
    pub fn from_agent_access(code: &str) -> Self {
        match code {
            "session_revoked" => Self::SessionRevoked,
            "session_expired" => Self::SessionExpired,
            "unauthorized" | "stale_generation" => Self::Unauthorized,
            "unsupported_api_version" => Self::UnsupportedApiVersion,
            "overloaded" => Self::Overloaded,
            _ => Self::Internal,
        }
    }
    pub fn status(self) -> u16 {
        match self {
            Self::SessionRevoked | Self::SessionExpired | Self::Unauthorized => 401,
            Self::UnknownCommand => 404,
            Self::InvalidRequest => 400,
            Self::Internal => 500,
            Self::Overloaded => 503,
            Self::UnsupportedApiVersion => 426,
            _ => 403,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refusal {
    pub code: RefusalCode,
    pub command: String,
    /// The operation the command needs.
    pub grant: Option<String>,
    /// Grants of the same family the caller holds, with their scopes.
    pub held: Vec<Grant>,
    pub message: String,
    /// Always `not_applied`: a refusal is decided before any effect.
    pub effect: String,
    pub request_id: Option<Uuid>,
}
fn held_text(held: &[Grant]) -> String {
    if held.is_empty() {
        return "no related grant".into();
    }
    held.iter()
        .map(|g| format!("{} @ {}", g.operation, g.scope))
        .collect::<Vec<_>>()
        .join(", ")
}
const ASK: &str = "Use `aq question ask` to request it.";
impl Refusal {
    fn new(code: RefusalCode, command: &str, message: String) -> Self {
        Self {
            code,
            command: command.into(),
            grant: None,
            held: Vec::new(),
            message,
            effect: "not_applied".into(),
            request_id: None,
        }
    }
    fn with_grant(mut self, grant: &str, held: Vec<Grant>) -> Self {
        self.grant = Some(grant.into());
        self.held = held;
        self
    }
    fn denied_grant(
        required: RequiredGrant<'_>,
        held: Vec<Grant>,
        code: RefusalCode,
        message: String,
    ) -> Self {
        Self::new(code, required.command, message).with_grant(required.operation, held)
    }
    pub fn grant_missing(required: RequiredGrant<'_>, held: Vec<Grant>) -> Self {
        let message = format!(
            "This agent does not hold the `{}` grant that `{}` needs (holds: {}). {ASK}",
            required.operation,
            required.command,
            held_text(&held)
        );
        Self::denied_grant(required, held, RefusalCode::GrantMissing, message)
    }
    pub fn scope_denied(required: RequiredGrant<'_>, held: Vec<Grant>) -> Self {
        let message = format!(
            "This agent holds `{}` only as {}; the target of `{}` is outside that scope. {ASK}",
            required.operation,
            held_text(&held),
            required.command
        );
        Self::denied_grant(required, held, RefusalCode::ScopeDenied, message)
    }
    /// The parent of an agent's create is not recorded as a task of the
    /// caller's project in the Graph (`aq:project_id`).
    pub fn parent_outside_project(command: &str, grant: &str, held: Vec<Grant>) -> Self {
        let message = "The parent task is not recorded as a task of this project (its Graph aq:project_id does not match), so a subtask cannot be created under it.".into();
        Self::new(RefusalCode::ScopeDenied, command, message).with_grant(grant, held)
    }
    pub fn owner_only(request: RequiredGrant<'_>) -> Self {
        let command = request.command;
        let grant = request.operation;
        let message =
            format!("`{command}` is an owner-only command and can never be granted to an agent.");
        Self::new(RefusalCode::OwnerOnly, command, message).with_grant(grant, Vec::new())
    }
    pub fn quota_exceeded(command: &str, grant: &str, held: Vec<Grant>, limit: &str) -> Self {
        let message = format!(
            "This agent reached the `{limit}` limit of its `{grant}` grant for `{command}`. {ASK}"
        );
        Self::new(RefusalCode::QuotaExceeded, command, message).with_grant(grant, held)
    }
    /// A command-surface error that names no grant.
    pub fn error(code: RefusalCode, command: &str, message: &str) -> Self {
        Self::new(code, command, message.into())
    }
    pub fn with_request(mut self, request_id: Uuid) -> Self {
        self.request_id = Some(request_id);
        self
    }
    /// The §6.8 body.
    pub fn body(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("refusal encodes")
    }
    /// The closed agent-access-1 `ErrorResponse` for a `/v1/agent/*` client
    /// that did not opt in to the structured shape: the [`RefusalCode::legacy`]
    /// code, with this refusal's message naming the grant.
    pub fn legacy_body(&self, request_id: Uuid) -> serde_json::Value {
        serde_json::json!({"api_version":1,"request_id":request_id,"operation_id":null,"code":self.code.legacy(),
            "message":self.message,"effect":"not_applied","retryable":false,"recovery":null})
    }
}
