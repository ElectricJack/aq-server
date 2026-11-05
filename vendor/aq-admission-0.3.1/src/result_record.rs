//! Immutable report acknowledgement, deliberately separate from policy eligibility
//! and resource/graph settlement. Existing ResultReport transport bytes stay fixed.
use crate::*;
use transport::{ResultReport, StopDisposition, TransportDocument};
pub const MAX_BYTES: usize = 65_536;
fn canonical(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(value).map_err(|_| Invalid("result encoding"))?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| Invalid("result encoding"))
}
pub fn validate_report(report: &ResultReport) -> Result<()> {
    TransportDocument::Result(Box::new(report.clone())).validate()?;
    require(
        matches!(report.stop.disposition, StopDisposition::Stopped { .. }),
        "recording requires post-Started stopped owner",
    )?;
    require(
        canonical(report)?.len() <= MAX_BYTES,
        "result report byte bound",
    )
}
pub fn decode_report(bytes: &[u8]) -> Result<ResultReport> {
    require(bytes.len() <= MAX_BYTES, "result input byte bound")?;
    let report: ResultReport =
        serde_json::from_slice(bytes).map_err(|_| Invalid("result report JSON"))?;
    validate_report(&report)?;
    Ok(report)
}
pub fn report_digest(report: &ResultReport) -> Result<Digest> {
    validate_report(report)?;
    Ok(Digest::of(&canonical(report)?))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Recorded,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub report_id: Uuid,
    pub binding: LaunchBinding,
    pub report_digest: Digest,
    pub expected_outcome: ArtifactRef,
    pub stopped_receipt: ArtifactRef,
    pub recorded_at_ms: u64,
    pub disposition: Disposition,
}
impl Receipt {
    pub fn validate_for(&self, report: &ResultReport) -> Result<()> {
        validate_report(report)?;
        require(
            self.schema_version == 1
                && self.operation_id == report.operation_id
                && self.report_id == report.report_id
                && self.binding == report.binding
                && self.report_digest == report_digest(report)?,
            "result receipt identity/digest",
        )?;
        revision(self.recorded_at_ms)?;
        artifact(&self.expected_outcome)?;
        artifact(&self.stopped_receipt)?;
        require(
            self.expected_outcome.id.as_str() == "aq.expected-outcome"
                && self.expected_outcome.version.as_str() == "1"
                && self.stopped_receipt.id.as_str()
                    == format!("aq.native-stopped/{}", report.binding.attempt_id)
                && self.stopped_receipt.version.as_str() == "1",
            "result receipt owner reference",
        )
    }
    /// A content address to the retained acknowledgment, never approval evidence.
    pub fn artifact(&self, report: &ResultReport) -> Result<ArtifactRef> {
        self.validate_for(report)?;
        Ok(ArtifactRef {
            id: Text::new(format!("aq.result-record/{}", self.report_id))
                .map_err(|_| Invalid("result receipt address"))?,
            version: Text::new("1").map_err(|_| Invalid("result receipt version"))?,
            digest: Digest::of(&canonical(self)?),
        })
    }
}
pub fn decode_receipt(bytes: &[u8], report: &ResultReport) -> Result<Receipt> {
    require(bytes.len() <= MAX_BYTES, "result receipt byte bound")?;
    let receipt: Receipt =
        serde_json::from_slice(bytes).map_err(|_| Invalid("result receipt JSON"))?;
    receipt.validate_for(report)?;
    Ok(receipt)
}
pub fn receipt_schema() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(Receipt)).expect("result record receipt schema");
    schema["$comment"] = serde_json::json!(
        "Historical report-recording acknowledgment only. This grants no result eligibility, source/effect release, execution or graph-completion authority. Owner references must be resolved through actual owners."
    );
    schema
}
