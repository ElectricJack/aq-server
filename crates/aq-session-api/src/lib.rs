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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    validate_contract(&doc)?;
    Ok(doc)
}

fn check_dimensions(columns: u16, rows: u16) -> Result<(), String> {
    check(
        (1..=1000).contains(&columns) && (1..=1000).contains(&rows),
        "dimensions must be 1..1000",
    )
}

fn check(condition: bool, message: &str) -> Result<(), String> {
    condition.then_some(()).ok_or_else(|| message.to_owned())
}

fn validate_contract(doc: &ContractDocument) -> Result<(), String> {
    use aq_session_protocol::Validate;
    let terminal = match doc {
        ContractDocument::ProfileValidate(_) | ContractDocument::ProfileExplain(_) => return Ok(()),
        ContractDocument::ProfileExplanation(r) => return r.check(),
        ContractDocument::Watch(r) => &r.terminal,
        ContractDocument::WatchView(r) => return r.check(),
        ContractDocument::AcquireControl(r) => &r.terminal,
        ContractDocument::Input(r) => {
            check_controller_generation(r.expected_controller_generation)?;
            check(
                (1..=4096).contains(&r.bytes.len()),
                "input must contain 1..4096 bytes",
            )?;
            &r.terminal
        }
        ContractDocument::Resize(r) => {
            check(
                r.expected_controller_generation > 0 && r.sequence > 0,
                "controller and resize sequence must be positive",
            )?;
            check_dimensions(r.columns, r.rows)?;
            &r.terminal
        }
        ContractDocument::ResolveInput(r) => {
            check_controller_generation(r.expected_controller_generation)?;
            &r.terminal
        }
        ContractDocument::InputReceipt(r) => {
            check(
                r.disposition != InputDisposition::DeliveryUnknown || r.input_paused,
                "unknown input must pause input",
            )?;
            &r.terminal
        }
        ContractDocument::Message(r) => {
            check(
                (1..=4096).contains(&r.text.len()),
                "message must contain 1..4096 UTF-8 bytes",
            )?;
            return r.session.validate().map_err(|e| e.to_string());
        }
    };
    terminal.validate().map_err(|e| e.to_string())
}

fn check_controller_generation(generation: u64) -> Result<(), String> {
    check(generation > 0, "controller generation must be positive")
}

impl ProfileExplanation {
    fn check(&self) -> Result<(), String> {
        if self.schema_version != API_VERSION {
            return Err("unsupported schema version".into());
        }
        let missing_required =
            self.omissions.iter().any(|o| o.required) || !self.denied_requirements.is_empty();
        check(
            !self.valid || !missing_required,
            "valid explanation denies required content",
        )?;
        self.check_budget()?;
        if self.valid && self.selection.selected_candidate_id.is_none() {
            return Err("valid explanation requires selection".into());
        }
        unique(
            self.selection
                .candidates
                .iter()
                .map(|c| c.candidate_id.clone()),
        )?;
        unique(self.flattened_modules.iter().map(|m| m.identity.clone()))?;
        self.check_permissions()?;
        self.selection.check()
    }

    fn check_budget(&self) -> Result<(), String> {
        check(
            self.estimated_tokens.is_some() == self.tokenizer_version.is_some(),
            "token estimate requires tokenizer provenance",
        )?;
        check(
            self.required_bytes
                .checked_add(self.optional_bytes)
                .is_some_and(|n| n <= 16_777_216),
            "context budget exceeds 16 MiB",
        )
    }

    fn check_permissions(&self) -> Result<(), String> {
        for permissions in [
            &self.requested_permissions,
            &self.effective_permissions,
            &self.denied_requirements,
        ] {
            unique(
                permissions
                    .iter()
                    .map(|p| format!("{}\0{}", p.operation, p.scope_reference)),
            )?;
        }
        for p in &self.effective_permissions {
            if !self.requested_permissions.contains(p) {
                return Err("effective permission was not requested".into());
            }
            if self.denied_requirements.contains(p) {
                return Err("permission is both effective and denied".into());
            }
        }
        Ok(())
    }
}
impl Selection {
    fn check(&self) -> Result<(), String> {
        if let Some(id) = &self.selected_candidate_id {
            let selected = self.candidates.iter().find(|c| &c.candidate_id == id);
            check(
                selected.is_some_and(|c| matches!(c.eligibility, Eligibility::Eligible)),
                "selection must identify one eligible candidate",
            )?;
        }
        let observations = self.candidates.iter().flat_map(|c| &c.observations);
        for observation in observations {
            if observation.sequence == 0 || observation.expires_at_ms <= observation.observed_at_ms
            {
                return Err("observation expiry precedes observation".into());
            }
        }
        Ok(())
    }
}
impl WatchView {
    fn check(&self) -> Result<(), String> {
        use aq_session_protocol::Validate;
        self.terminal.validate().map_err(|e| e.to_string())?;
        check_dimensions(self.columns, self.rows)?;
        let lost = self.truncated || !self.gaps.is_empty();
        match self.coverage {
            Coverage::Complete => check(!lost, "complete coverage cannot contain loss")?,
            Coverage::Gapped => check(lost, "gapped coverage requires loss marker")?,
            Coverage::Unknown => {}
        }
        for gap in &self.gaps {
            gap.validate().map_err(|e| e.to_string())?;
            if gap.terminal != self.terminal {
                return Err("gap targets different terminal".into());
            }
        }
        Ok(())
    }
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
        serde_json::Value::String(s) => validate_string(key.unwrap_or(""), s)?,
        serde_json::Value::Number(n)
            if positive_field(key.unwrap_or("")) && n.as_u64() == Some(0) =>
        {
            return Err("required revision/time must be positive".into());
        }
        _ => {}
    }
    Ok(())
}
fn validate_string(k: &str, s: &str) -> Result<(), String> {
    check(!s.trim().is_empty(), "empty or control-bearing reference")?;
    if k != "text" {
        check(
            !s.chars().any(char::is_control),
            "empty or control-bearing reference",
        )?;
    }
    if digest_field(k) {
        check(valid_digest(s), "digest must be lowercase SHA256")?;
    }
    if k.ends_with("_id") {
        check(
            !Uuid::parse_str(s).is_ok_and(|id| id.is_nil()),
            "nil identity",
        )?;
    }
    if k == "scope_reference" {
        check(valid_scope(s), "scope must be a bounded logical reference")?;
    }
    Ok(())
}

fn valid_scope(s: &str) -> bool {
    s.len() <= 512 && s.contains(':') && s.split(':').all(|part| !part.is_empty())
}

fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn positive_field(name: &str) -> bool {
    name.ends_with("_at_ms")
        || matches!(
            name,
            "expected_project_revision"
                | "expected_session_revision"
                | "session_revision"
                | "generation"
                | "assignment_generation"
                | "epoch"
        )
}

fn digest_field(name: &str) -> bool {
    name == "digest" || name.ends_with("_digest")
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
    walk_schema(&mut v);
    if let Some(definitions) = v.get_mut("$defs").and_then(|v| v.as_object_mut()) {
        for (name, schema) in definitions {
            bound_definition(name, schema);
        }
    }
    v
}
fn walk_schema(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(o) => {
            for v in o.values_mut() {
                walk_schema(v);
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
                walk_schema(v);
            }
        }
        _ => {}
    }
}
fn bound_definition(name: &str, schema: &mut serde_json::Value) {
    if let Some(fields) = schema.get_mut("properties").and_then(|v| v.as_object_mut()) {
        for (field, schema) in fields {
            bound_field(name, field, schema);
        }
    }
}

fn bound_field(definition_name: &str, name: &str, field: &mut serde_json::Value) {
    if field.get("format").and_then(|v| v.as_str()) == Some("uuid") {
        field["not"] = serde_json::json!({"const":"00000000-0000-0000-0000-000000000000"});
    }
    match name {
        "expected_controller_generation" if definition_name != "AcquireControlRequest" => {
            field["minimum"] = 1.into()
        }
        "columns" | "rows" => {
            field["minimum"] = 1.into();
            field["maximum"] = 1000.into();
        }
        name if positive_field(name) => field["minimum"] = 1.into(),
        _ => {}
    }
    if digest_field(name) && field.is_object() {
        field["pattern"] = "^[0-9a-f]{64}$".into();
        field["minLength"] = 64.into();
        field["maxLength"] = 64.into();
        field["x-maxUtf8Bytes"] = 64.into();
    }
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
