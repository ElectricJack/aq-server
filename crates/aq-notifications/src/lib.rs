//! Bounded notification delivery plans. Persistence and authorization belong to
//! the adapter; a pure state transition is not durable dispatch permission.
mod delivery;
pub mod discord;
use aq_lifecycle_api::{Digest, Scope, Validate};
pub use delivery::{AttemptOutcome, Schedule};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid notification contract")]
    Invalid,
    #[error("notification scope or route changed")]
    ScopeChanged,
    #[error("notification state conflicts")]
    Conflict,
    #[error("notification is not due or has expired")]
    NotDue,
    #[error("notification retry budget exhausted")]
    Exhausted,
}
fn require(ok: bool) -> Result<()> {
    if ok { Ok(()) } else { Err(Error::Invalid) }
}
fn bare_endpoint(url: &reqwest::Url) -> bool {
    url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}
fn positive(v: u64) -> bool {
    (1..=i64::MAX as u64).contains(&v)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub message_id: Uuid,
    pub revision: u64,
    pub scope: Scope,
    pub recipient_user_id: Uuid,
    /// Deliberately supplied external summary, never a transcript export.
    pub summary: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}
impl Message {
    pub fn validate(&self) -> Result<()> {
        self.scope.validate().map_err(|_| Error::Invalid)?;
        require(
            !self.message_id.is_nil()
                && !self.recipient_user_id.is_nil()
                && positive(self.revision)
                && self.valid_lifetime()
                && self.valid_summary(),
        )
    }
    fn valid_lifetime(&self) -> bool {
        [self.created_at_ms, self.expires_at_ms]
            .into_iter()
            .all(positive)
            && self.expires_at_ms > self.created_at_ms
            && self.expires_at_ms - self.created_at_ms <= 604_800_000
    }
    fn valid_summary(&self) -> bool {
        !self.summary.trim().is_empty()
            && self.summary.len() <= 4096
            && self.summary.encode_utf16().count() <= 1600
            && !self
                .summary
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub max_elapsed_ms: u64,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    /// New, explicitly authorized retry intents may accept duplicate delivery.
    pub allow_duplicate_risk: bool,
}
impl RetryPolicy {
    pub fn validate(&self) -> Result<()> {
        require(
            (1..=8).contains(&self.max_attempts)
                && (1000..=86_400_000).contains(&self.max_elapsed_ms)
                && (250..=60_000).contains(&self.initial_delay_ms)
                && (self.initial_delay_ms..=3_600_000).contains(&self.max_delay_ms),
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub route_id: Uuid,
    pub revision: u64,
    pub scope: Scope,
    pub recipient_user_id: Uuid,
    /// Logical protected configuration key; no URL or token.
    pub endpoint_key: String,
    pub endpoint_revision: u64,
    pub enabled: bool,
    pub retry: RetryPolicy,
}
impl Route {
    pub fn validate(&self) -> Result<()> {
        self.scope.validate().map_err(|_| Error::Invalid)?;
        self.retry.validate()?;
        require(
            !self.route_id.is_nil()
                && !self.recipient_user_id.is_nil()
                && positive(self.revision)
                && positive(self.endpoint_revision)
                && (1..=128).contains(&self.endpoint_key.len())
                && self
                    .endpoint_key
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub content: String,
}
impl Payload {
    pub fn for_message(message: &Message, public_origin: &str) -> Result<Self> {
        message.validate()?;
        let u = reqwest::Url::parse(public_origin).map_err(|_| Error::Invalid)?;
        require(
            u.scheme() == "https"
                && u.host_str().is_some()
                && bare_endpoint(&u)
                && u.path() == "/"
                && public_origin.len() <= 256,
        )?;
        let content = format!(
            "{}\n\n{}/api/notifications/{}",
            message.summary,
            public_origin.trim_end_matches('/'),
            message.message_id
        );
        let result = Self { content };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        require(
            (1..=8192).contains(&self.content.len()) && self.content.encode_utf16().count() <= 2000,
        )
    }
    pub fn wire(&self) -> Result<serde_json::Value> {
        self.validate()?;
        Ok(
            serde_json::json!({"content":self.content,"allowed_mentions":{"parse":[],"users":[],"roles":[],"replied_user":false},"flags":4,"tts":false}),
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Status {
    Queued,
    Dispatching {
        attempt_id: Uuid,
        started_at_ms: u64,
    },
    AcceptedByTransport {
        attempt_id: Uuid,
        message_id: String,
        accepted_at_ms: u64,
    },
    DeliveryUnknown {
        attempt_id: Uuid,
        since_ms: u64,
    },
    RetryScheduled {
        next_attempt_ms: u64,
    },
    PermanentlyFailed {
        reason: FailureReason,
    },
    Suppressed {
        reason: SuppressionReason,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    InvalidPayload,
    DestinationUnavailable,
    RetryExhausted,
    ProviderDelayTooLong,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppressionReason {
    ScopeRevoked,
    RouteChanged,
    MessageExpired,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    pub delivery_id: Uuid,
    pub message: Message,
    pub route: Route,
    pub payload: Payload,
    pub immutable_digest: Digest,
    pub attempts: u32,
    pub first_attempt_ms: Option<u64>,
    pub last_transition_ms: u64,
    pub duplicate_risk_retries: u32,
    pub status: Status,
}
/// Trusted current facts, acquired under the owning service's route/scope lock.
/// Neither a serialized request nor a transport response supplies these values.
pub struct DispatchAuthority<'a> {
    pub route: &'a Route,
    pub recipient_can_view: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportOutcome {
    Accepted { message_id: String },
    RateLimited { retry_after_ms: u64, global: bool },
    Permanent(FailureReason),
    Unknown,
}
pub fn snowflake(value: &str) -> bool {
    (1..=20).contains(&value.len())
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|v| v > 0)
}
// Central human API requests; worker credentials cannot use these endpoints.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRequest {
    pub operation_id: Uuid,
    pub expected_revision: u64,
    pub route: Route,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendRequest {
    pub operation_id: Uuid,
    pub message_id: Uuid,
    pub revision: u64,
    pub scope: Scope,
    pub recipient_user_id: Uuid,
    pub summary: String,
    pub ttl_ms: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryRequest {
    pub operation_id: Uuid,
    pub delivery_id: Uuid,
    pub expected_revision: u64,
    pub accept_duplicate_risk: bool,
}
