//! Future authenticated human API contracts. These values confer no authority.
//! Handlers must derive the principal from authentication and recheck current
//! tenant/project/session authorization; validation is only structural.
pub use aq_session_protocol::{InputDisposition, SessionIdentity, TerminalIdentity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const API_VERSION: u32 = 1;
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRequest {
    pub catalog_digest: String,
    pub agent_profile: String,
    pub project_binding_id: Uuid,
    pub expected_project_revision: u64,
    pub task_context_revision: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub identity: String,
    pub digest: String,
    pub revision: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Permission {
    pub operation: String,
    /// Logical scope reference, never a physical credential or secret value.
    pub scope_reference: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Omission {
    pub source: Source,
    pub reason_code: String,
    pub required: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Eligibility {
    Eligible,
    Unavailable,
    Unsupported,
    StaleUnknown,
    PolicyDenied,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub source: Source,
    pub sequence: u64,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub account_profile_reference: String,
    pub endpoint_reference: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub candidate_id: String,
    pub provider: String,
    pub model: String,
    pub harness: String,
    pub reasoning_configuration_reference: String,
    pub eligibility: Eligibility,
    pub reason_codes: Vec<String>,
    pub observations: Vec<Observation>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub intelligence_profile: Source,
    pub selected_candidate_id: Option<String>,
    pub snapshot_digest: String,
    pub decision_trace: Vec<String>,
    pub candidates: Vec<Candidate>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OverrideExplanation {
    pub field: String,
    pub source: Source,
    pub reason_code: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    Gapped,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchView {
    pub terminal: TerminalIdentity,
    pub subscription_id: Uuid,
    pub session_revision: u64,
    pub expires_at_ms: u64,
    pub last_observed_at_ms: u64,
    pub disconnected: bool,
    pub coverage: Coverage,
    pub truncated: bool,
    pub snapshot_id: Uuid,
    pub snapshot_reference: Source,
    pub renderer_reference: Source,
    pub through_sequence: u64,
    pub columns: u16,
    pub rows: u16,
    pub gaps: Vec<aq_session_protocol::OutputGap>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileExplanation {
    pub schema_version: u32,
    pub request: ProfileRequest,
    pub valid: bool,
    pub agent_profile: Source,
    pub execution_profile: Source,
    /// API-01 graph authorization remains a separate identity and digest.
    pub graph_access_profile_digest: String,
    pub flattened_modules: Vec<Source>,
    pub overrides: Vec<OverrideExplanation>,
    pub requested_permissions: Vec<Permission>,
    pub effective_permissions: Vec<Permission>,
    pub denied_requirements: Vec<Permission>,
    pub omissions: Vec<Omission>,
    pub reference_material: Vec<Source>,
    pub required_bytes: u64,
    pub optional_bytes: u64,
    pub estimated_tokens: Option<u64>,
    pub tokenizer_version: Option<String>,
    pub selection: Selection,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchRequest {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub expected_session_revision: u64,
    pub expires_at_ms: u64,
    pub after_sequence: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AcquireControlRequest {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub expected_session_revision: u64,
    pub expected_controller_generation: u64,
    pub expires_at_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputRequest {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub lease_id: Uuid,
    pub expected_controller_generation: u64,
    pub expected_session_revision: u64,
    pub expires_at_ms: u64,
    #[schemars(length(min = 1, max = 4096))]
    pub bytes: Vec<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub escape_key: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResizeRequest {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub lease_id: Uuid,
    pub expected_controller_generation: u64,
    pub expected_session_revision: u64,
    pub expires_at_ms: u64,
    pub sequence: u64,
    pub columns: u16,
    pub rows: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageRequest {
    pub session: SessionIdentity,
    pub operation_id: Uuid,
    pub expected_session_revision: u64,
    pub expires_at_ms: u64,
    #[schemars(length(min = 1, max = 4096))]
    pub text: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveInputRequest {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub uncertain_operation_id: Uuid,
    pub lease_id: Uuid,
    pub expected_controller_generation: u64,
    pub expected_session_revision: u64,
    pub fresh_snapshot_id: Uuid,
    pub fresh_capture_request_id: Uuid,
    pub fresh_snapshot_sequence: u64,
    pub expires_at_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputReceipt {
    pub operation_id: Uuid,
    pub terminal: TerminalIdentity,
    pub disposition: InputDisposition,
    pub input_paused: bool,
    pub observed_at_ms: u64,
}
/// Publication envelope, not a routing or authorization mechanism.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "contract",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ContractDocument {
    ProfileValidate(ProfileRequest),
    ProfileExplain(ProfileRequest),
    ProfileExplanation(Box<ProfileExplanation>),
    Watch(WatchRequest),
    WatchView(Box<WatchView>),
    AcquireControl(AcquireControlRequest),
    Input(InputRequest),
    Resize(ResizeRequest),
    Message(MessageRequest),
    ResolveInput(ResolveInputRequest),
    InputReceipt(InputReceipt),
}

/// Bounded deserialization for untrusted requests and scoped explanation output.
/// This does not establish ownership, tenant membership, current revisions or grants.
pub fn decode(bytes: &[u8]) -> Result<ContractDocument, String> {
    if bytes.len() > 262_144 {
        return Err("document exceeds 256 KiB".into());
    }
    let doc: ContractDocument = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let value = serde_json::to_value(&doc).map_err(|e| e.to_string())?;
    bound_value(&value, 0, false)?;
    validate_fields(&value, None)?;
    match &doc {
        ContractDocument::Input(r) if r.expected_controller_generation == 0 => {
            return Err("controller generation must be positive".into());
        }
        ContractDocument::Resize(r) if r.expected_controller_generation == 0 || r.sequence == 0 => {
            return Err("controller and resize sequence must be positive".into());
        }
        ContractDocument::ResolveInput(r) if r.expected_controller_generation == 0 => {
            return Err("controller generation must be positive".into());
        }
        ContractDocument::InputReceipt(r)
            if matches!(r.disposition, InputDisposition::DeliveryUnknown) && !r.input_paused =>
        {
            return Err("unknown input must pause input".into());
        }
        ContractDocument::Input(r) if r.bytes.is_empty() || r.bytes.len() > 4096 => {
            return Err("input must contain 1..4096 bytes".into());
        }
        ContractDocument::Message(r) if r.text.is_empty() || r.text.len() > 4096 => {
            return Err("message must contain 1..4096 UTF-8 bytes".into());
        }
        ContractDocument::Resize(r)
            if r.columns == 0 || r.rows == 0 || r.columns > 1000 || r.rows > 1000 =>
        {
            return Err("dimensions must be 1..1000".into());
        }
        ContractDocument::ProfileExplanation(r) => {
            if r.schema_version != API_VERSION {
                return Err("unsupported schema version".into());
            }
            if r.valid
                && (r.omissions.iter().any(|o| o.required) || !r.denied_requirements.is_empty())
            {
                return Err("valid explanation denies required content".into());
            }
            if r.estimated_tokens.is_some() != r.tokenizer_version.is_some() {
                return Err("token estimate requires tokenizer provenance".into());
            }
            if r.required_bytes
                .checked_add(r.optional_bytes)
                .filter(|n| *n <= 16_777_216)
                .is_none()
            {
                return Err("context budget exceeds 16 MiB".into());
            }
            if r.valid && r.selection.selected_candidate_id.is_none() {
                return Err("valid explanation requires selection".into());
            }
            unique(
                r.selection
                    .candidates
                    .iter()
                    .map(|c| c.candidate_id.clone()),
            )?;
            unique(r.flattened_modules.iter().map(|m| m.identity.clone()))?;
            for permissions in [
                &r.requested_permissions,
                &r.effective_permissions,
                &r.denied_requirements,
            ] {
                unique(
                    permissions
                        .iter()
                        .map(|p| format!("{}\0{}", p.operation, p.scope_reference)),
                )?;
            }
            for p in &r.effective_permissions {
                if !r
                    .requested_permissions
                    .iter()
                    .any(|q| p.operation == q.operation && p.scope_reference == q.scope_reference)
                {
                    return Err("effective permission was not requested".into());
                }
                if r.denied_requirements
                    .iter()
                    .any(|q| p.operation == q.operation && p.scope_reference == q.scope_reference)
                {
                    return Err("permission is both effective and denied".into());
                }
            }
            if let Some(id) = &r.selection.selected_candidate_id
                && r.selection
                    .candidates
                    .iter()
                    .filter(|c| {
                        &c.candidate_id == id && matches!(c.eligibility, Eligibility::Eligible)
                    })
                    .count()
                    != 1
            {
                return Err("selection must identify one eligible candidate".into());
            }
            for c in &r.selection.candidates {
                for o in &c.observations {
                    if o.sequence == 0 || o.expires_at_ms <= o.observed_at_ms {
                        return Err("observation expiry precedes observation".into());
                    }
                }
            }
        }
        _ => {}
    }
    use aq_session_protocol::Validate;
    match &doc {
        ContractDocument::Watch(r) => r.terminal.validate(),
        ContractDocument::WatchView(r) => {
            r.terminal.validate().map_err(|e| e.to_string())?;
            if r.columns == 0 || r.rows == 0 || r.columns > 1000 || r.rows > 1000 {
                return Err("dimensions must be 1..1000".into());
            }
            if matches!(r.coverage, Coverage::Complete) && (r.truncated || !r.gaps.is_empty()) {
                return Err("complete coverage cannot contain loss".into());
            }
            if matches!(r.coverage, Coverage::Gapped) && r.gaps.is_empty() && !r.truncated {
                return Err("gapped coverage requires loss marker".into());
            }
            for gap in &r.gaps {
                gap.validate().map_err(|e| e.to_string())?;
                if gap.terminal != r.terminal {
                    return Err("gap targets different terminal".into());
                }
            }
            Ok(())
        }
        ContractDocument::AcquireControl(r) => r.terminal.validate(),
        ContractDocument::Input(r) => r.terminal.validate(),
        ContractDocument::Resize(r) => r.terminal.validate(),
        ContractDocument::ResolveInput(r) => r.terminal.validate(),
        ContractDocument::InputReceipt(r) => r.terminal.validate(),
        ContractDocument::Message(r) => r.session.validate(),
        _ => Ok(()),
    }
    .map_err(|e| e.to_string())?;
    Ok(doc)
}
fn unique(values: impl Iterator<Item = String>) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err("duplicate identity or permission".into());
        }
    }
    Ok(())
}
fn validate_fields(v: &serde_json::Value, key: Option<&str>) -> Result<(), String> {
    match v {
        serde_json::Value::Object(o) => {
            for (k, v) in o {
                validate_fields(v, Some(k))?;
            }
        }
        serde_json::Value::Array(a) => {
            for v in a {
                validate_fields(v, None)?;
            }
        }
        serde_json::Value::String(s) => {
            let k = key.unwrap_or("");
            if s.trim().is_empty() || (k != "text" && s.chars().any(char::is_control)) {
                return Err("empty or control-bearing reference".into());
            }
            if (k == "digest" || k.ends_with("_digest"))
                && (s.len() != 64
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
            {
                return Err("digest must be lowercase SHA256".into());
            }
            if k.ends_with("_id") && Uuid::parse_str(s).is_ok_and(|id| id.is_nil()) {
                return Err("nil identity".into());
            }
            if k == "scope_reference"
                && (s.len() > 512 || !s.contains(':') || s.split(':').any(str::is_empty))
            {
                return Err("scope must be a bounded logical reference".into());
            }
        }
        serde_json::Value::Number(n) => {
            let k = key.unwrap_or("");
            if (k.ends_with("_at_ms")
                || k == "expected_project_revision"
                || k == "expected_session_revision"
                || k == "session_revision"
                || k == "generation"
                || k == "assignment_generation"
                || k == "epoch")
                && n.as_u64() == Some(0)
            {
                return Err("required revision/time must be positive".into());
            }
        }
        _ => {}
    }
    Ok(())
}
fn bound_value(v: &serde_json::Value, depth: usize, input_bytes: bool) -> Result<(), String> {
    if depth > 24 {
        return Err("nesting exceeds 24".into());
    }
    match v {
        serde_json::Value::String(s) if s.len() > 4096 => {
            return Err("string exceeds 4096 UTF-8 bytes".into());
        }
        serde_json::Value::Array(a) => {
            let limit = if input_bytes { 4096 } else { 128 };
            if a.len() > limit {
                return Err("array exceeds bound".into());
            }
            for v in a {
                bound_value(v, depth + 1, false)?;
            }
        }
        serde_json::Value::Object(o) => {
            for (k, v) in o {
                bound_value(v, depth + 1, k == "bytes")?;
            }
        }
        _ => {}
    }
    Ok(())
}
/// Independently versioned JSON Schema with conservative publication bounds.
pub fn schema() -> serde_json::Value {
    let mut v = serde_json::to_value(schemars::schema_for!(ContractDocument)).unwrap();
    fn walk(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(o) => {
                for v in o.values_mut() {
                    walk(v);
                }
                if o.get("type").and_then(|v| v.as_str()) == Some("string") {
                    o.entry("maxLength").or_insert(4096.into());
                    o.entry("minLength").or_insert(1.into());
                    o.insert(
                        "x-maxUtf8Bytes".into(),
                        o.get("maxLength").cloned().unwrap_or(4096.into()),
                    );
                }
                if o.get("type").and_then(|v| v.as_str()) == Some("array") {
                    o.entry("maxItems").or_insert(128.into());
                }
            }
            serde_json::Value::Array(a) => {
                for v in a {
                    walk(v);
                }
            }
            _ => {}
        }
    }
    walk(&mut v);
    if let Some(defs) = v.get_mut("$defs").and_then(|v| v.as_object_mut()) {
        for (definition_name, def) in defs.iter_mut() {
            if let Some(properties) = def.get_mut("properties").and_then(|v| v.as_object_mut()) {
                for (name, field) in properties {
                    if field.get("format").and_then(|v| v.as_str()) == Some("uuid") {
                        field["not"] =
                            serde_json::json!({"const":"00000000-0000-0000-0000-000000000000"});
                    }
                    if name == "expected_controller_generation"
                        && definition_name != "AcquireControlRequest"
                    {
                        field["minimum"] = 1.into();
                    }
                    if name == "columns" || name == "rows" {
                        field["minimum"] = 1.into();
                        field["maximum"] = 1000.into();
                    }
                    if name.ends_with("_at_ms")
                        || name == "expected_project_revision"
                        || name == "expected_session_revision"
                        || name == "session_revision"
                        || name == "generation"
                        || name == "assignment_generation"
                        || name == "epoch"
                    {
                        field["minimum"] = 1.into();
                    }
                    if (name == "digest" || name.ends_with("_digest")) && field.is_object() {
                        field["pattern"] = "^[0-9a-f]{64}$".into();
                        field["minLength"] = 64.into();
                        field["maxLength"] = 64.into();
                        field["x-maxUtf8Bytes"] = 64.into();
                    }
                }
            }
        }
    }
    v
}

impl aq_session_protocol::Validate for ContractDocument {
    fn validate(&self) -> Result<(), aq_session_protocol::ContractError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|e| aq_session_protocol::ContractError::Json(e.to_string()))?;
        decode(&bytes)
            .map(|_| ())
            .map_err(aq_session_protocol::ContractError::Json)
    }
}
/// Same canonicalization as the pinned worker contract; use this digest when
/// durably binding operation IDs. This function does not store replay history.
pub fn request_digest(
    doc: &ContractDocument,
) -> Result<aq_session_protocol::Digest, aq_session_protocol::ContractError> {
    aq_session_protocol::canonical_digest(doc)
}
