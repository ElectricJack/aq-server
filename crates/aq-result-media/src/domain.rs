use crate::{Invalid, Result, Validate, limits::*, require};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Id(Uuid);
impl Id {
    pub fn new(value: Uuid) -> Result<Self> {
        require(!value.is_nil(), "UUID")?;
        Ok(Self(value))
    }
    pub fn parse(value: &str) -> Result<Self> {
        let id = Uuid::parse_str(value).map_err(|_| Invalid::Field("UUID"))?;
        require(id.to_string() == value, "canonical UUID")?;
        Self::new(id)
    }
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}
impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Digest(String);
impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
    }
    pub fn parse(value: &str) -> Result<Self> {
        require(
            value
                .strip_prefix("sha256:")
                .is_some_and(|s| lower_hex(s, 64)),
            "digest",
        )?;
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ReportRevision(String);
impl ReportRevision {
    pub fn parse(value: &str) -> Result<Self> {
        require(lower_hex(value, 64), "report revision")?;
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for ReportRevision {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
fn required_option<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::deserialize(d)
}
fn timestamp(value: u64) -> Result<()> {
    require((1..=MAX_SAFE_MILLIS).contains(&value), "timestamp")
}
fn label(value: &str, max: usize) -> Result<()> {
    require(
        !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control),
        "label",
    )
}
fn filename(value: &str) -> Result<()> {
    label(value, 256)?;
    require(
        !value.contains(['/', '\\']) && !matches!(value, "." | ".."),
        "filename",
    )
}
fn decode<T: serde::de::DeserializeOwned + Validate>(bytes: &[u8], maximum: usize) -> Result<T> {
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(Invalid::Limit("JSON bytes"));
    }
    // Derived Serde struct visitors also accept positional sequences. This wire
    // contract requires objects; its only array is the root MediaPage.items.
    // Decode from original bytes again so duplicate known keys still fail.
    let shape: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| Invalid::Field("closed JSON"))?;
    require(shape.is_object(), "JSON object")?;
    object_shapes(&shape, true)?;
    let value: T = serde_json::from_slice(bytes).map_err(|_| Invalid::Field("closed JSON"))?;
    value.validate()?;
    Ok(value)
}
fn object_shapes(value: &serde_json::Value, root: bool) -> Result<()> {
    if let serde_json::Value::Object(fields) = value {
        for (name, value) in fields {
            if root && name == "items" && value.is_array() {
                for item in value.as_array().expect("checked array") {
                    require(item.is_object(), "media item object")?;
                    object_shapes(item, false)?;
                }
            } else {
                require(!value.is_array(), "JSON object shape")?;
                object_shapes(value, false)?;
            }
        }
    }
    Ok(())
}
pub fn canonical_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(value).map_err(|_| Invalid::Field("JSON encoding"))?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| Invalid::Field("JSON encoding"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Contract {
    #[serde(rename = "human-result-media-1")]
    HumanResultMedia1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportCoverage {
    #[serde(rename = "recorded_legacy_native_only")]
    RecordedLegacyNativeOnly,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mime {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "video/mp4")]
    Mp4,
}
impl Mime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Mp4 => "video/mp4",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderingPolicy {
    #[serde(rename = "aq.png.rgba8-srgb-v1")]
    Rgba8SrgbV1,
    /// Structurally preflighted ISO-BMFF clip retained byte-for-byte; the
    /// preview variant is the verified preview of a retained poster image.
    #[serde(rename = "aq.mp4.passthrough-v1")]
    Mp4PassthroughV1,
}
impl RenderingPolicy {
    pub fn media(self) -> MediaKind {
        match self {
            Self::Rgba8SrgbV1 => MediaKind::Image,
            Self::Mp4PassthroughV1 => MediaKind::Video,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rgba8SrgbV1 => "aq.png.rgba8-srgb-v1",
            Self::Mp4PassthroughV1 => "aq.mp4.passthrough-v1",
        }
    }
}
/// Media kind of an upload intent. Image is the default and is omitted from
/// canonical encodings so pre-clip requests and receipts are byte-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    #[default]
    Image,
    Video,
}
impl MediaKind {
    pub fn is_image(&self) -> bool {
        *self == Self::Image
    }
    pub fn policy(self) -> RenderingPolicy {
        match self {
            Self::Image => RenderingPolicy::Rgba8SrgbV1,
            Self::Video => RenderingPolicy::Mp4PassthroughV1,
        }
    }
    /// Static per-kind input bound before admission limits apply.
    pub fn max_input_bytes(self) -> u64 {
        match self {
            Self::Image => MAX_INPUT_BYTES as u64,
            Self::Video => MAX_CLIP_BYTES as u64,
        }
    }
    pub fn admitted_input_bytes(self, limits: &Limits) -> u64 {
        match self {
            Self::Image => limits.max_input_bytes,
            Self::Video => limits.max_clip_bytes,
        }
    }
    pub fn input_mime(self) -> Mime {
        match self {
            Self::Image => Mime::Png,
            Self::Video => Mime::Mp4,
        }
    }
}
/// Video request shape: a clip always names an already-retained poster image
/// on the same report revision; images never name one.
fn poster_shape(media: MediaKind, poster: Option<&Id>, own: &Id) -> Result<()> {
    require(
        match media {
            MediaKind::Image => poster.is_none(),
            MediaKind::Video => poster.is_some_and(|p| p != own),
        },
        "clip poster",
    )
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "lowercase", deny_unknown_fields)]
pub enum ObjectId {
    Sha1 { hex: String },
    Sha256 { hex: String },
}
impl Validate for ObjectId {
    fn validate(&self) -> Result<()> {
        require(
            match self {
                Self::Sha1 { hex } => lower_hex(hex, 40),
                Self::Sha256 { hex } => lower_hex(hex, 64),
            },
            "source object",
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBinding {
    pub repository_id: Id,
    pub base: ObjectId,
}
/// Well-formed provenance only. The daemon must resolve the actual retained
/// legacy-native owner; unsupported native reports cannot be an empty media page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportBinding {
    pub coverage: ReportCoverage,
    pub report_id: Id,
    pub report_revision: ReportRevision,
    pub project_id: Id,
    pub task_id: Id,
    pub attempt_id: Id,
    pub source: SourceBinding,
}
impl Validate for ReportBinding {
    fn validate(&self) -> Result<()> {
        self.source.base.validate()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Viewport {
    pub width_css: u32,
    pub height_css: u32,
    pub dpr_milli: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub scenario: String,
    pub captured_at_ms: u64,
    pub viewport: Viewport,
    pub environment: String,
    #[serde(deserialize_with = "required_option")]
    pub claimed_source: Option<ObjectId>,
}
impl Validate for Capture {
    fn validate(&self) -> Result<()> {
        label(&self.scenario, 128)?;
        label(&self.environment, 128)?;
        timestamp(self.captured_at_ms)?;
        require(
            (1..=4096).contains(&self.viewport.width_css)
                && (1..=4096).contains(&self.viewport.height_css)
                && (100..=8000).contains(&self.viewport.dpr_milli),
            "viewport",
        )?;
        if let Some(source) = &self.claimed_source {
            source.validate()?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub digest: Digest,
    pub bytes: u64,
    pub filename: String,
}
impl Validate for Input {
    /// Kind-independent outer bound; owners apply the per-kind bound.
    fn validate(&self) -> Result<()> {
        require(
            (1..=MAX_INPUT_BYTES.max(MAX_CLIP_BYTES) as u64).contains(&self.bytes),
            "input bytes",
        )?;
        filename(&self.filename)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadCreate {
    pub operation_id: Id,
    pub input_digest: Digest,
    pub input_bytes: u64,
    pub filename: String,
    #[serde(deserialize_with = "required_option")]
    pub capture: Option<Capture>,
    #[serde(default, skip_serializing_if = "MediaKind::is_image")]
    pub media: MediaKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster_artifact_id: Option<Id>,
}
impl UploadCreate {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        decode(bytes, MAX_JSON_BYTES)
    }
    pub fn validate_with_limits(&self, limits: &Limits) -> Result<()> {
        self.validate()?;
        limits.validate()?;
        require(
            self.input_bytes <= self.media.admitted_input_bytes(limits),
            "admitted input limit",
        )
    }
}
impl Validate for UploadCreate {
    fn validate(&self) -> Result<()> {
        require(
            (1..=self.media.max_input_bytes()).contains(&self.input_bytes),
            "input bytes",
        )?;
        poster_shape(
            self.media,
            self.poster_artifact_id.as_ref(),
            &self.operation_id,
        )?;
        filename(&self.filename)?;
        if let Some(capture) = &self.capture {
            capture.validate()?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}
impl Empty {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        decode(bytes, MAX_JSON_BYTES)
    }
}
impl Validate for Empty {
    fn validate(&self) -> Result<()> {
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub digest: Digest,
    pub bytes: u64,
    pub mime: Mime,
    pub width: u32,
    pub height: u32,
}
impl Validate for Variant {
    fn validate(&self) -> Result<()> {
        rgba_len(self.width, self.height)?;
        let max = match self.mime {
            Mime::Png => MAX_RENDER_BYTES,
            Mime::Mp4 => MAX_CLIP_BYTES,
        };
        require((1..=max as u64).contains(&self.bytes), "variant bytes")
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clip {
    pub duration_ms: u64,
    pub poster_artifact_id: Id,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variants {
    pub preview: Variant,
    pub render: Variant,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: u32,
    pub artifact_id: Id,
    pub binding: ReportBinding,
    pub uploader_id: Id,
    pub input: Input,
    #[serde(deserialize_with = "required_option")]
    pub capture: Option<Capture>,
    pub rendering_policy: RenderingPolicy,
    pub variants: Variants,
    pub retained_at_ms: u64,
    pub retain_until_ms: u64,
    /// Present exactly for `aq.mp4.passthrough-v1`; omitted for images so
    /// canonical image receipts (and their digests) are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<Clip>,
}
impl Receipt {
    pub fn media(&self) -> MediaKind {
        self.rendering_policy.media()
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        let bytes = canonical_bytes(self)?;
        require(bytes.len() <= MAX_JSON_BYTES, "receipt bytes")?;
        Ok(Digest::of(&bytes))
    }
}
impl Validate for Receipt {
    fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "receipt schema")?;
        self.binding.validate()?;
        self.input.validate()?;
        if let Some(capture) = &self.capture {
            capture.validate()?;
        }
        self.variants.render.validate()?;
        self.variants.preview.validate()?;
        let p = &self.variants.preview;
        let r = &self.variants.render;
        require(
            p.mime == Mime::Png && p.bytes <= MAX_PREVIEW_BYTES as u64,
            "preview",
        )?;
        match (self.rendering_policy, &self.clip) {
            (RenderingPolicy::Rgba8SrgbV1, None) => require(
                r.mime == Mime::Png
                    && self.input.bytes <= MAX_INPUT_BYTES as u64
                    && (p.width, p.height) == preview_dimensions(r.width, r.height)?,
                "image variants",
            )?,
            (RenderingPolicy::Mp4PassthroughV1, Some(clip)) => require(
                r.mime == Mime::Mp4
                    && r.digest == self.input.digest
                    && r.bytes == self.input.bytes
                    && p.width <= PREVIEW_EDGE
                    && p.height <= PREVIEW_EDGE
                    && (1..=MAX_CLIP_DURATION_MS).contains(&clip.duration_ms)
                    && clip.poster_artifact_id != self.artifact_id,
                "clip variants",
            )?,
            _ => return Err(Invalid::Field("rendering policy")),
        }
        timestamp(self.retained_at_ms)?;
        timestamp(self.retain_until_ms)?;
        require(
            self.retain_until_ms > self.retained_at_ms
                && self.retain_until_ms - self.retained_at_ms <= MAX_RETENTION_MS,
            "retention",
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityState {
    Available,
    Expired,
    Missing,
    Withheld,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityReason {
    RetentionExpired,
    FilesMissing,
    OwnerWithheld,
    IntegrityFailed,
    StorageUnavailable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Availability {
    pub state: AvailabilityState,
    pub observed_at_ms: u64,
    #[serde(deserialize_with = "required_option")]
    pub reason: Option<AvailabilityReason>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub receipt: Receipt,
    pub receipt_digest: Digest,
    pub availability: Availability,
}
impl Validate for Artifact {
    fn validate(&self) -> Result<()> {
        require(
            self.receipt.digest()? == self.receipt_digest,
            "receipt digest",
        )?;
        let a = &self.availability;
        timestamp(a.observed_at_ms)?;
        require(
            a.observed_at_ms >= self.receipt.retained_at_ms,
            "availability time",
        )?;
        require(
            match a.state {
                AvailabilityState::Available => {
                    a.reason.is_none() && a.observed_at_ms < self.receipt.retain_until_ms
                }
                AvailabilityState::Expired => {
                    a.reason == Some(AvailabilityReason::RetentionExpired)
                        && a.observed_at_ms >= self.receipt.retain_until_ms
                }
                AvailabilityState::Missing => a.reason == Some(AvailabilityReason::FilesMissing),
                AvailabilityState::Withheld => a.reason == Some(AvailabilityReason::OwnerWithheld),
                AvailabilityState::Failed => matches!(
                    a.reason,
                    Some(
                        AvailabilityReason::IntegrityFailed
                            | AvailabilityReason::StorageUnavailable
                    )
                ),
            },
            "availability",
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_input_bytes: u64,
    pub max_render_bytes: u64,
    pub max_preview_bytes: u64,
    pub max_pixels: u64,
    pub max_dimension: u32,
    pub max_result_artifacts: u32,
    pub upload_ttl_ms: u64,
    pub retention_ms: u64,
    /// Defaults keep limits stored before clip support decodable.
    #[serde(default = "default_clip_bytes")]
    pub max_clip_bytes: u64,
    #[serde(default = "default_clip_duration_ms")]
    pub max_clip_duration_ms: u64,
}
fn default_clip_bytes() -> u64 {
    MAX_CLIP_BYTES as u64
}
fn default_clip_duration_ms() -> u64 {
    MAX_CLIP_DURATION_MS
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: MAX_INPUT_BYTES as u64,
            max_render_bytes: MAX_RENDER_BYTES as u64,
            max_preview_bytes: MAX_PREVIEW_BYTES as u64,
            max_pixels: MAX_PIXELS,
            max_dimension: MAX_DIMENSION,
            max_result_artifacts: MAX_REPORT_ARTIFACTS as u32,
            upload_ttl_ms: MAX_UPLOAD_TTL_MS,
            retention_ms: MAX_RETENTION_MS,
            max_clip_bytes: MAX_CLIP_BYTES as u64,
            max_clip_duration_ms: MAX_CLIP_DURATION_MS,
        }
    }
}
impl Validate for Limits {
    fn validate(&self) -> Result<()> {
        require(
            (1..=MAX_INPUT_BYTES as u64).contains(&self.max_input_bytes)
                && (1..=MAX_RENDER_BYTES as u64).contains(&self.max_render_bytes)
                && (1..=MAX_PREVIEW_BYTES as u64).contains(&self.max_preview_bytes)
                && (1..=MAX_PIXELS).contains(&self.max_pixels)
                && (1..=MAX_DIMENSION).contains(&self.max_dimension)
                && (1..=MAX_REPORT_ARTIFACTS as u32).contains(&self.max_result_artifacts)
                && (1..=MAX_UPLOAD_TTL_MS).contains(&self.upload_ttl_ms)
                && (1..=MAX_RETENTION_MS).contains(&self.retention_ms)
                && (1..=MAX_CLIP_BYTES as u64).contains(&self.max_clip_bytes)
                && (1..=MAX_CLIP_DURATION_MS).contains(&self.max_clip_duration_ms)
                && self.upload_ttl_ms < self.retention_ms,
            "limits",
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadState {
    Reserved,
    Received,
    Finalizing,
    Retained,
    Rejected,
    Expired,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryReason {
    Busy,
    Expired,
    Rejected,
    AlreadyRetained,
    AccessChanged,
    StorageChanged,
    ClockRegression,
    RetryBudgetExhausted,
    InputRequired,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    pub put_allowed: bool,
    pub finalize_allowed: bool,
    #[serde(deserialize_with = "required_option")]
    pub reason: Option<RecoveryReason>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    InvalidPng,
    UnsupportedPng,
    InputMismatch,
    PixelLimit,
    ProcessingLimit,
    StorageFailure,
    InvalidVideo,
    UnsupportedVideo,
    PosterUnavailable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadStatus {
    pub contract: Contract,
    pub operation_id: Id,
    pub actor_id: Id,
    pub request_digest: Digest,
    pub binding: ReportBinding,
    pub input: Input,
    #[serde(deserialize_with = "required_option")]
    pub capture: Option<Capture>,
    pub state: UploadState,
    pub created_at_ms: u64,
    pub upload_expires_at_ms: u64,
    pub retain_until_ms: u64,
    pub recovery: Recovery,
    #[serde(deserialize_with = "required_option")]
    pub rejection: Option<Rejection>,
    #[serde(deserialize_with = "required_option")]
    pub artifact: Option<Artifact>,
    pub limits: Limits,
    #[serde(default, skip_serializing_if = "MediaKind::is_image")]
    pub media: MediaKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster_artifact_id: Option<Id>,
}
impl UploadStatus {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        decode(bytes, MAX_PAGE_BYTES)
    }
}
impl Validate for UploadStatus {
    fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        self.input.validate()?;
        self.limits.validate()?;
        require(
            self.input.bytes <= self.media.max_input_bytes()
                && self.input.bytes <= self.media.admitted_input_bytes(&self.limits),
            "admitted input limit",
        )?;
        poster_shape(
            self.media,
            self.poster_artifact_id.as_ref(),
            &self.operation_id,
        )?;
        if let Some(c) = &self.capture {
            c.validate()?;
        }
        timestamp(self.created_at_ms)?;
        timestamp(self.upload_expires_at_ms)?;
        timestamp(self.retain_until_ms)?;
        require(
            self.upload_expires_at_ms > self.created_at_ms
                && self.upload_expires_at_ms - self.created_at_ms <= self.limits.upload_ttl_ms
                && self.retain_until_ms > self.upload_expires_at_ms
                && self.retain_until_ms - self.created_at_ms <= self.limits.retention_ms,
            "intent deadlines",
        )?;
        require(
            (self.state == UploadState::Retained) == self.artifact.is_some(),
            "retained artifact",
        )?;
        if let Some(a) = &self.artifact {
            a.validate()?;
            let r = &a.receipt;
            require(
                r.artifact_id == self.operation_id
                    && r.uploader_id == self.actor_id
                    && r.binding == self.binding
                    && r.input == self.input
                    && r.capture == self.capture
                    && r.media() == self.media
                    && r.clip.as_ref().map(|c| c.poster_artifact_id) == self.poster_artifact_id
                    && r.retained_at_ms >= self.created_at_ms
                    && r.retained_at_ms < self.upload_expires_at_ms
                    && r.retain_until_ms == self.retain_until_ms,
                "intent receipt",
            )?;
        }
        require(
            (self.state == UploadState::Rejected) == self.rejection.is_some(),
            "rejection",
        )?;
        match self.state {
            UploadState::Retained | UploadState::Rejected | UploadState::Expired => require(
                !self.recovery.put_allowed && !self.recovery.finalize_allowed,
                "terminal recovery",
            )?,
            UploadState::Reserved => require(!self.recovery.finalize_allowed, "input required")?,
            UploadState::Finalizing => require(!self.recovery.put_allowed, "active writer")?,
            UploadState::Received => {}
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaPage {
    pub contract: Contract,
    pub server_time_ms: u64,
    pub binding: ReportBinding,
    pub limits: Limits,
    pub items: Vec<Artifact>,
}
impl MediaPage {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        decode(bytes, MAX_PAGE_BYTES)
    }
}
impl Validate for MediaPage {
    fn validate(&self) -> Result<()> {
        timestamp(self.server_time_ms)?;
        self.binding.validate()?;
        self.limits.validate()?;
        require(
            self.items.len() <= self.limits.max_result_artifacts as usize,
            "media page length",
        )?;
        let mut previous = None;
        for a in &self.items {
            a.validate()?;
            require(a.receipt.binding == self.binding, "media page scope")?;
            let id = a.receipt.artifact_id;
            require(previous.is_none_or(|p| p < id), "media page order")?;
            previous = Some(id);
            require(
                a.availability.observed_at_ms <= self.server_time_ms,
                "media observation time",
            )?;
            if a.availability.state == AvailabilityState::Available {
                let r = &a.receipt.variants.render;
                let p = &a.receipt.variants.preview;
                require(
                    self.server_time_ms < a.receipt.retain_until_ms
                        && r.bytes <= self.limits.max_render_bytes
                        && p.bytes <= self.limits.max_preview_bytes
                        && a.receipt.input.bytes
                            <= a.receipt.media().admitted_input_bytes(&self.limits)
                        && a.receipt
                            .clip
                            .as_ref()
                            .is_none_or(|c| c.duration_ms <= self.limits.max_clip_duration_ms)
                        && u64::from(r.width) * u64::from(r.height) <= self.limits.max_pixels
                        && r.width.max(r.height) <= self.limits.max_dimension,
                    "current media limits",
                )?;
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentError {
    MediaExpired,
    MediaMissing,
    MediaWithheld,
    MediaFailed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentFailure {
    pub contract: Contract,
    pub error: ContentError,
    pub message: String,
}
impl Validate for ContentFailure {
    fn validate(&self) -> Result<()> {
        label(&self.message, 128)
    }
}
