//! Projections of exact retained native result claims (metadata only). No
//! claim, stop receipt or process outcome establishes acceptance/settlement.
use serde::{Deserialize, Serialize};
use uuid::Uuid;
pub const COVERAGE: &str = "recorded_legacy_native_only";
pub const NATIVE_V2_COVERAGE: &str = "recorded_native_v2";
#[derive(Clone, Copy, Debug, Deserialize, schemars::JsonSchema)]
pub enum ReadProfile {
    #[serde(rename = "recorded_native_v2")]
    RecordedNativeV2,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResultOrigin {
    LegacyPrepareV1,
    PhaseExecutionV2,
    PhaseRefreshExecutionV1,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultMetadata {
    /// Only opt-in v2 pages expose origin. Default v1 retains its old field set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ResultOrigin>,
    pub report_id: Uuid,
    pub revision: String,
    pub project_id: Uuid,
    pub task_id: Uuid,
    pub attempt_id: Uuid,
    pub recorded_at_ms: u64,
    pub conclusion: String,
    pub process_outcome: String,
    pub current_report_head: bool,
    pub head_report_id: Uuid,
    pub supersedes: Option<Uuid>,
    pub repository_id: Uuid,
    pub source_base: aq_admission::ObjectId,
    pub source_receipt_ids: Vec<Uuid>,
    pub pending_effect_ids: Vec<Uuid>,
}
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultPage {
    pub schema_version: u32,
    pub server_time_ms: u64,
    pub coverage: String,
    pub items: Vec<ResultMetadata>,
    pub next_after: Option<Uuid>,
    /// True when a listed report could not be read; `unavailable` names each.
    /// Both are omitted from a complete page, so its bytes are unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<UnavailableResult>,
}
/// One report of a listed page that was refused or failed validation. It is
/// reported in place of metadata, so one bad report cannot hide the others.
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnavailableResult {
    pub report_id: Uuid,
    pub reason: String,
}
impl ResultMetadata {
    /// The metadata of one validated recorded result. `origin` is exposed only
    /// to opt-in v2 pages; the default v1 page retains its old field set.
    pub fn record(
        profile: Option<ReadProfile>,
        recorded: Recorded<'_>,
        (head_report_id, current_report_head): (Uuid, bool),
    ) -> Self {
        use aq_admission::transport::ProcessOutcome as Outcome;
        let (origin, report, receipt, spec) = recorded;
        Self {
            origin: profile.map(|_| origin),
            report_id: report.report_id,
            revision: receipt.report_digest.as_str().into(),
            project_id: spec.lifecycle.project_id,
            task_id: spec.task.task_id,
            attempt_id: report.binding.attempt_id,
            recorded_at_ms: receipt.recorded_at_ms,
            conclusion: report.conclusion.as_str().into(),
            process_outcome: match report.outcome {
                Outcome::Success => "success",
                Outcome::Failed => "failed",
                Outcome::Cancelled => "cancelled",
                Outcome::Expired => "expired",
                Outcome::Unknown => "unknown",
            }
            .into(),
            current_report_head,
            head_report_id,
            supersedes: report.supersedes,
            repository_id: spec.source.repository_id,
            source_base: spec.source.resolved_object,
            source_receipt_ids: report.source_receipt_ids,
            pending_effect_ids: report.pending_effect_ids,
        }
    }
}
impl ResultOrigin {
    /// The origin a recorded result kind names.
    pub fn from_kind(kind: &str) -> Option<Self> {
        Some(match kind {
            "legacy_prepare_v1" => Self::LegacyPrepareV1,
            "phase_execution_v2" => Self::PhaseExecutionV2,
            "phase_refresh_execution_v1" => Self::PhaseRefreshExecutionV1,
            _ => return None,
        })
    }
    /// Whether a page of `profile` covers this origin: the default page covers
    /// legacy reports only, and the v2 profile covers every origin.
    pub fn is_covered_by(self, profile: Option<ReadProfile>) -> bool {
        profile.is_some() || self == Self::LegacyPrepareV1
    }
}
impl ResultMetadata {
    /// Whether this is not the project's report or the exact revision asked for.
    pub fn is_outside(&self, project: Option<Uuid>, exact: Option<(Uuid, &str)>) -> bool {
        project.is_some_and(|p| p != self.project_id)
            || exact.is_some_and(|(_, revision)| revision != self.revision)
    }
}
impl UnavailableResult {
    pub fn new(report_id: Uuid, reason: &str) -> Self {
        Self {
            report_id,
            reason: reason.into(),
        }
    }
}
/// A page being read: the metadata of each report read, the reports that
/// could not be, and the last report visited (the page's keyset cursor).
pub struct Collector {
    profile: Option<ReadProfile>,
    project: Option<Uuid>,
    revision: Option<String>,
    items: Vec<ResultMetadata>,
    unavailable: Vec<UnavailableResult>,
    last: Option<Uuid>,
}
impl Collector {
    /// A page of `profile` in `project`; an exact read also names its `revision`.
    pub fn new(
        profile: Option<ReadProfile>,
        project: Option<Uuid>,
        revision: Option<&str>,
    ) -> Self {
        Self {
            profile,
            project,
            revision: revision.map(Into::into),
            items: Vec::new(),
            unavailable: Vec::new(),
            last: None,
        }
    }
    /// Reports that `id` could not be read, for `reason`.
    pub fn unavailable(&mut self, id: Uuid, reason: &str) {
        self.last = Some(id);
        self.unavailable.push(UnavailableResult::new(id, reason));
    }
    /// Adds the metadata of `id`'s validated result; false (and nothing
    /// added) when it is not in the page's project or at its exact revision.
    pub fn record(&mut self, id: Uuid, recorded: Recorded<'_>, head: (Uuid, bool)) -> bool {
        self.last = Some(id);
        let value = ResultMetadata::record(self.profile, recorded, head);
        let exact = self.revision.as_deref().map(|revision| (id, revision));
        let outside = value.is_outside(self.project, exact);
        if !outside {
            self.items.push(value);
        }
        !outside
    }
    /// The page; it is incomplete when any listed report is `unavailable`, and
    /// continues after the last report visited when `more` follow.
    pub fn finish(self, server_time_ms: u64, more: bool) -> ResultPage {
        ResultPage {
            schema_version: 1,
            server_time_ms,
            coverage: self.profile.map_or(COVERAGE, |_| NATIVE_V2_COVERAGE).into(),
            items: self.items,
            next_after: self.last.filter(|_| more),
            incomplete: !self.unavailable.is_empty(),
            unavailable: self.unavailable,
        }
    }
}
/// A validated recorded result: its origin, report, receipt and spec.
pub type Recorded<'a> = (
    ResultOrigin,
    aq_admission::transport::ResultReport,
    &'a aq_admission::result_record::Receipt,
    aq_admission::execution_spec::ProfiledExecutionSpec,
);
/// The query of a result page: all optional, `limit` 1 to 25.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub profile: Option<ReadProfile>,
    pub project_id: Option<Uuid>,
    pub after: Option<Uuid>,
    pub limit: Option<u32>,
}
impl ResultPage {
    /// The agent `result.list` answer for `project`: coverage is explicit, and
    /// `complete` is false when any listed report is `unavailable`.
    pub fn agent_json(&self, project: Uuid) -> serde_json::Value {
        serde_json::json!({
            "api_version": 1, "project_id": project, "coverage": self.coverage,
            "complete": !self.incomplete, "unavailable": self.unavailable,
            "items": self.items, "next_after": self.next_after,
        })
    }
}
