//! Native startup delivery profile. Constituent Startup references stay pinned
//! in the manifest, but only this already-rendered UTF-8 payload is delivered.
//! Validation is structural: qualification and retention owners remain trusted
//! daemon decisions, and no value here grants permission to execute.
use crate::*;
use aq_session_protocol::ContextDelivery;

pub const MAX_PAYLOAD_BYTES: u64 = 1_048_576;
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputSpan {
    pub artifact: ArtifactRef,
    pub offset: u64,
    pub bytes: u64,
}
/// The renderer qualification binds the precise producer rendering and the
/// adapter's byte-for-byte consumption, including any implicit model overhead.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub schema_version: u32,
    pub payload: ArtifactRef,
    pub renderer_qualification: ArtifactRef,
    #[schemars(length(min = 1, max = 128))]
    pub startup_inputs: Vec<InputSpan>,
    #[schemars(range(min = 1, max = 1_048_576))]
    pub payload_bytes: u64,
    pub payload_tokens_upper_bound: u64,
    pub implicit_tokens_upper_bound: u64,
}
impl Descriptor {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "startup delivery version")?;
        artifact(&self.payload)?;
        artifact(&self.renderer_qualification)?;
        require(
            (1..=MAX_PAYLOAD_BYTES).contains(&self.payload_bytes)
                && !self.startup_inputs.is_empty()
                && self.startup_inputs.len() <= 128,
            "startup shape bound",
        )?;
        let mut end = 0;
        let mut seen = std::collections::BTreeSet::new();
        for input in &self.startup_inputs {
            artifact(&input.artifact)?;
            require(
                seen.insert((input.artifact.id.clone(), input.artifact.version.clone())),
                "duplicate startup input",
            )?;
            require(input.bytes > 0 && input.offset >= end, "startup span order")?;
            end = input
                .offset
                .checked_add(input.bytes)
                .ok_or(Invalid("startup span overflow"))?;
            require(end <= self.payload_bytes, "startup span bound")?;
        }
        require(
            self.payload_tokens_upper_bound > 0
                && self
                    .payload_tokens_upper_bound
                    .checked_add(self.implicit_tokens_upper_bound)
                    .is_some(),
            "startup token bound",
        )
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        let digest = aq_session_protocol::canonical_digest(self)
            .map_err(|_| Invalid("startup descriptor encoding"))?;
        Ok(ArtifactRef {
            id: Text::new(format!("aq.startup-delivery/{}", digest.as_str()))
                .map_err(|_| Invalid("startup descriptor identity"))?,
            version: Text::new("1").map_err(|_| Invalid("startup descriptor version"))?,
            digest,
        })
    }
    pub fn validate_against(
        &self,
        offer: &ProfiledOffer,
        retained: &[RetainedArtifact],
        now: u64,
    ) -> Result<()> {
        self.validate()?;
        verify_offer_artifacts(offer, retained, now)?;
        require(self.schema_version == 1, "startup delivery version")?;
        let descriptor = self.artifact()?;
        let descriptor_bytes = aq_session_protocol::canonical_bytes(self)
            .map_err(|_| Invalid("startup descriptor encoding"))?;
        require(
            offer
                .manifest
                .modules
                .iter()
                .filter(|reference| reference.id.as_str().starts_with("aq.startup-delivery/"))
                .count()
                == 1
                && offer.manifest.modules.contains(&descriptor),
            "manifest pins exactly one startup descriptor",
        )?;
        require(
            retained.iter().any(|r| {
                r.artifact == descriptor
                    && r.kind == ArtifactKind::Definition
                    && r.bytes == descriptor_bytes.len() as u64
            }),
            "retained exact startup descriptor",
        )?;
        artifact(&self.payload)?;
        artifact(&self.renderer_qualification)?;
        require(
            (1..=MAX_PAYLOAD_BYTES).contains(&self.payload_bytes),
            "startup payload byte bound",
        )?;
        require(
            !self.startup_inputs.is_empty() && self.startup_inputs.len() <= 128,
            "startup input bound",
        )?;
        let plan = &offer.manifest.context;
        let expected: Vec<_> = plan
            .references
            .iter()
            .filter(|r| r.delivery == ContextDelivery::Startup)
            .collect();
        require(
            expected.len() == self.startup_inputs.len()
                && expected
                    .iter()
                    .zip(&self.startup_inputs)
                    .all(|(a, b)| a.artifact == b.artifact && a.bytes == b.bytes),
            "exact ordered startup input coverage",
        )?;
        let mut seen = std::collections::BTreeSet::new();
        let mut previous_end = 0;
        for input in &self.startup_inputs {
            artifact(&input.artifact)?;
            require(
                seen.insert((input.artifact.id.clone(), input.artifact.version.clone())),
                "duplicate startup input",
            )?;
            let end = input
                .offset
                .checked_add(input.bytes)
                .ok_or(Invalid("startup span overflow"))?;
            require(
                input.bytes > 0 && input.offset >= previous_end && end <= self.payload_bytes,
                "ordered nonoverlapping startup spans",
            )?;
            previous_end = end;
        }
        require(
            !plan.references.iter().any(|r| r.artifact == self.payload),
            "rendered payload is separate from constituent references",
        )?;
        require(
            retained.iter().any(|r| {
                r.artifact == self.payload
                    && r.kind == ArtifactKind::Context
                    && r.bytes == self.payload_bytes
            }),
            "retained rendered startup payload",
        )?;
        require(
            retained.iter().any(|r| {
                r.artifact == self.renderer_qualification && r.kind == ArtifactKind::Definition
            }),
            "retained renderer qualification",
        )?;
        let tokens = self
            .payload_tokens_upper_bound
            .checked_add(self.implicit_tokens_upper_bound)
            .ok_or(Invalid("startup token overflow"))?;
        require(
            self.payload_tokens_upper_bound > 0
                && tokens <= plan.startup_token_budget
                && self.payload_bytes <= plan.startup_byte_budget,
            "complete startup delivery budget",
        )?;
        require(
            tokens
                .checked_add(plan.reserved_output_tokens)
                .is_some_and(|total| total <= plan.context_window_tokens),
            "startup output reservation",
        )
    }
    /// The worker fetches and verifies this one body, then supplies these exact
    /// bytes without worker-added prefixes, suffixes, interpolation or constituents.
    /// Slice checks prove designated input presence, not the meaning of bytes
    /// outside the spans; all framing remains producer/renderer qualification.
    pub fn verify_payload(&self, bytes: &[u8]) -> Result<()> {
        self.validate()?;
        require(
            bytes.len() as u64 == self.payload_bytes
                && self.payload_bytes <= MAX_PAYLOAD_BYTES
                && !bytes.is_empty(),
            "startup actual byte bound",
        )?;
        require(std::str::from_utf8(bytes).is_ok(), "startup payload UTF-8")?;
        require(
            Digest::of(bytes) == self.payload.digest,
            "startup payload digest",
        )?;
        let mut previous_end = 0;
        for input in &self.startup_inputs {
            let end = input
                .offset
                .checked_add(input.bytes)
                .ok_or(Invalid("startup span overflow"))?;
            require(
                input.bytes > 0 && input.offset >= previous_end && end <= bytes.len() as u64,
                "startup span range",
            )?;
            let fragment = &bytes[input.offset as usize..end as usize];
            require(
                std::str::from_utf8(fragment).is_ok()
                    && Digest::of(fragment) == input.artifact.digest,
                "startup constituent bytes",
            )?;
            previous_end = end;
        }
        Ok(())
    }
}
/// Additive native consumption envelope. The frozen OfferedLaunch wire remains
/// useful for compatibility replay; this explicit profile is required by the
/// new producer and its qualified worker consumer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeOffer {
    pub schema_version: u32,
    pub offered: transport::OfferedLaunch,
    pub startup: Descriptor,
    /// Spec 0016 §5.2 (feature `delivery_playbook`): how this attempt fetches
    /// from and pushes to central aq-git. Absent when the server has no git
    /// endpoint configured or the project follows no named ref.
    #[cfg(feature = "delivery_playbook")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitGrant>,
}
/// The git grant of one attempt (spec 0016 §5.2). The node clones and pushes
/// through `endpoint` on the server's machine listener, sending its node
/// credential and `token` in the `AQ-Git-Token` header. The server, not the
/// node, tells aq-git the base, the task ref, its lease and the main ref.
#[cfg(feature = "delivery_playbook")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitGrant {
    /// `/v1/git/<repository_id>`, relative to the server's machine origin.
    pub endpoint: String,
    pub repository_id: Uuid,
    /// The commit the attempt starts from; every pushed commit descends from it.
    pub base: String,
    /// `refs/heads/aq/<task-id>`, the only ref the attempt may push.
    pub task_ref: String,
    /// The task ref's head in central aq-git when the offer was made; absent
    /// means the ref did not exist and a push creates it.
    pub lease: Option<String>,
    /// The followed default branch, advertised read-only.
    pub main_ref: String,
    /// Names the push receipt (`push.receipt {operation_id}`) of the attempt.
    pub operation_id: Uuid,
    /// Valid for this attempt, on this node, for this repository only. Added
    /// when the offer is delivered; never stored with the offer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}
#[cfg(feature = "delivery_playbook")]
impl GitGrant {
    pub fn validate(&self) -> Result<()> {
        let oid =
            |v: &str| v.len() == 40 && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        let branch = |v: &str| {
            v.starts_with("refs/heads/")
                && v.len() <= 1024
                && !v.contains("..")
                && !v
                    .bytes()
                    .any(|b| b <= b' ' || b == 127 || b"~^:?*[\\".contains(&b))
        };
        require(
            !self.repository_id.is_nil()
                && !self.operation_id.is_nil()
                && self.endpoint == format!("/v1/git/{}", self.repository_id)
                && oid(&self.base)
                && self.lease.as_deref().is_none_or(oid)
                && branch(&self.main_ref)
                && branch(&self.task_ref)
                && self.task_ref.starts_with("refs/heads/aq/")
                && self.task_ref != self.main_ref
                && self
                    .token
                    .as_ref()
                    .is_none_or(|t| !t.is_empty() && t.len() <= 256),
            "native offer git grant",
        )
    }
}
impl NativeOffer {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "native offer delivery version")?;
        #[cfg(feature = "delivery_playbook")]
        if let Some(git) = &self.git {
            git.validate()?;
        }
        self.offered.validate()?;
        self.startup.validate_against(
            &self.offered.offer,
            &self.offered.artifacts,
            self.offered.clock.server_time_ms,
        )
    }
}
pub fn decode_native_offer(bytes: &[u8]) -> Result<NativeOffer> {
    require(bytes.len() <= 4_194_304, "native offer frame bound")?;
    let offer: NativeOffer =
        serde_json::from_slice(bytes).map_err(|_| Invalid("native offer JSON shape"))?;
    offer.validate()?;
    Ok(offer)
}

impl aq_session_protocol::Validate for Descriptor {
    fn validate(&self) -> std::result::Result<(), aq_session_protocol::ContractError> {
        Descriptor::validate(self)
            .map_err(|_| aq_session_protocol::ContractError::Invalid("startup descriptor"))
    }
}

pub fn native_offer_schema() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(NativeOffer)).expect("native startup schema");
    fn bounds(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("type") == Some(&serde_json::json!("string")) {
                    map.entry("maxLength").or_insert(4096.into());
                    map.insert("x-utf8-maxBytes".into(), map["maxLength"].clone());
                }
                if map.get("type") == Some(&serde_json::json!("array")) {
                    map.entry("maxItems").or_insert(128.into());
                }
                for child in map.values_mut() {
                    bounds(child);
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    bounds(child);
                }
            }
            _ => {}
        }
    }
    bounds(&mut schema);
    if let Some(digest) = schema
        .get_mut("$defs")
        .and_then(|v| v.get_mut("Digest"))
        .and_then(serde_json::Value::as_object_mut)
    {
        digest.insert("minLength".into(), 64.into());
        digest.insert("maxLength".into(), 64.into());
        digest.insert("x-utf8-maxBytes".into(), 64.into());
        digest.insert("pattern".into(), "^[0-9a-f]{64}$".into());
    }
    schema["$comment"] = serde_json::json!(
        "Additive native startup delivery. Validate descriptor content-address pin in manifest.modules, retained roles/bytes, exact ordered Startup coverage, nonoverlapping byte spans, actual UTF8/slice/whole digests, checked token/output budgets and current retention in code. Qualified counting/renderer evidence and current execution authorization remain actual owner checks, not schema authority. Constituent Startup artifacts are delivered through this one payload and never independently appended. Frozen OfferedLaunch remains unchanged."
    );
    schema
}
