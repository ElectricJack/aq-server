use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Anchor {
    Result,
    ImageRegion {
        artifact_id: Uuid,
        #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
        digest: String,
        #[schemars(range(min = 0, max = 1))]
        x: f64,
        #[schemars(range(min = 0, max = 1))]
        y: f64,
        #[schemars(range(min = 0, max = 1))]
        width: f64,
        #[schemars(range(min = 0, max = 1))]
        height: f64,
    },
    VideoRange {
        artifact_id: Uuid,
        #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
        digest: String,
        start_ms: u64,
        end_ms: u64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevisionRef {
    pub result_id: Uuid,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub revision: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPin {
    pub artifact_id: Uuid,
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Viewport {
    pub width_css: u32,
    pub height_css: u32,
    pub dpr_milli: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequirement {
    pub id: String,
    pub policy_revision: String,
    pub kind: String,
    pub scenario: String,
    pub steps: Vec<String>,
    pub viewport: Viewport,
    pub environment: String,
    pub count: u16,
    pub required: bool,
    pub applicability: String,
    pub gate_binding: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapturePolicy {
    #[schemars(regex(pattern = "^(0|[1-9][0-9]{0,18})$"))]
    pub revision: String,
    pub requirements: Vec<CaptureRequirement>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Follow {
    pub operation_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    pub following: bool,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Comment {
    pub operation_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[schemars(length(min = 1, max = 16384))]
    pub body: String,
    pub anchor: Anchor,
    #[serde(default)]
    pub thread_id: Option<Uuid>,
    #[serde(default)]
    pub follow: bool,
    pub result_id: Uuid,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub revision: String,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Correction {
    pub operation_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[schemars(length(min = 1, max = 16384))]
    pub body: String,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resolve {
    pub operation_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    pub follow_up: RevisionRef,
    pub reason: String,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedbackDraft {
    pub operation_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    pub result_id: Uuid,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub revision: String,
    pub thread_id: Uuid,
    pub comment_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub comment_revision: String,
    #[schemars(length(min = 1, max = 200))]
    pub title: String,
    pub description: String,
    pub acceptance_criteria: String,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_project_revision: String,
    pub parent_id: Option<Uuid>,
    #[schemars(length(max = 20))]
    pub artifacts: Vec<ArtifactPin>,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedbackLink {
    pub operation_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    pub result_id: Uuid,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub revision: String,
    pub thread_id: Uuid,
    pub comment_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub comment_revision: String,
    pub project_id: Uuid,
    pub task_id: Uuid,
    #[schemars(length(max = 20))]
    pub artifacts: Vec<ArtifactPin>,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Waiver {
    pub operation_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub revision: String,
    pub requirement_id: String,
    pub policy_revision: String,
    pub reason: String,
}

#[derive(Clone)]
pub(crate) enum Command {
    Follow(Follow),
    Comment(Comment),
    Correction(Correction),
    Resolve(Resolve),
    Draft(FeedbackDraft),
    Link(FeedbackLink),
    Waiver(Waiver),
}
impl Command {
    pub fn id(&self) -> Uuid {
        match self {
            Self::Follow(v) => v.operation_id,
            Self::Comment(v) => v.operation_id,
            Self::Correction(v) => v.operation_id,
            Self::Resolve(v) => v.operation_id,
            Self::Draft(v) => v.operation_id,
            Self::Link(v) => v.operation_id,
            Self::Waiver(v) => v.operation_id,
        }
    }
    pub fn version(&self) -> &str {
        match self {
            Self::Follow(v) => &v.expected_version,
            Self::Comment(v) => &v.expected_version,
            Self::Correction(v) => &v.expected_version,
            Self::Resolve(v) => &v.expected_version,
            Self::Draft(v) => &v.expected_version,
            Self::Link(v) => &v.expected_version,
            Self::Waiver(v) => &v.expected_version,
        }
    }
}

/// Additive read contract revision; missing markers identify legacy v1 responses.
#[derive(Default, Serialize, Deserialize, JsonSchema)]
pub enum FeedbackReadVersion {
    #[default]
    #[serde(rename = "results-feedback-1")]
    V1,
    #[serde(rename = "results-feedback-1.1")]
    V1_1,
}

/// No submission owner exists yet. Links and resolution never imply submission.
#[derive(Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum FollowUpSubmission {
    #[default]
    Unsupported,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedbackOrigin {
    pub result_id: Uuid,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub revision: String,
    pub thread_id: Uuid,
    pub comment_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub comment_revision: String,
    #[schemars(length(max = 20))]
    pub artifacts: Vec<ArtifactPin>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedbackLinkProjection {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub task_id: Uuid,
    pub origin: FeedbackOrigin,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThreadResolution {
    pub resolved_by: Uuid,
    pub reason: String,
    pub follow_up: RevisionRef,
    pub follow_up_report_id: Uuid,
}

/// Result identity and principal preference, independent of mutable assessment.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultProjection {
    /// Direct principal preference, independent of thread opt-in membership.
    #[serde(default)]
    pub directly_following: bool,
    pub id: Uuid,
    pub version: String,
    pub project_id: Uuid,
    pub task_id: Uuid,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub current_revision: String,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultRevision {
    pub report_id: Uuid,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub revision: String,
    pub attempt_id: Uuid,
    pub source: Source,
    pub summary: String,
    pub submitted_at_ms: u64,
    pub parent_results: Vec<RevisionRef>,
    pub capture_policy: CapturePolicy,
    #[schemars(length(max = 20))]
    pub artifacts: Vec<ArtifactPin>,
    pub check_refs: Vec<Uuid>,
    pub evidence_refs: Vec<Uuid>,
    /// Compatibility projection of thread IDs explicitly resolved against this revision.
    /// This supplemental observation is not part of the immutable native report.
    pub addressed_feedback: Vec<Uuid>,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub repository_id: Uuid,
    pub base: aq_admission::ObjectId,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Evaluation {
    pub satisfied: bool,
    pub requirements: Vec<RequirementEvaluation>,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequirementEvaluation {
    pub requirement_id: String,
    pub policy_revision: String,
    pub required: bool,
    pub state: String,
    pub artifact_ids: Vec<Uuid>,
    pub waiver_id: Option<Uuid>,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultAssessment {
    pub version: String,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub result_revision: String,
    pub evidence: Evaluation,
    #[schemars(length(max = 20))]
    pub artifacts: Vec<ArtifactPin>,
    pub acceptance: String,
    pub observed_at_ms: u64,
    pub unresolved_review: bool,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Detail {
    #[serde(default)]
    pub feedback_read_version: FeedbackReadVersion,
    #[serde(default)]
    pub follow_up_submission: FollowUpSubmission,
    pub result: ResultProjection,
    pub revision: ResultRevision,
    pub assessment: ResultAssessment,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommentRevision {
    pub revision: String,
    pub body: String,
    pub author_id: Uuid,
    pub recorded_at_ms: i64,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommentProjection {
    #[serde(default)]
    pub feedback_links: Vec<FeedbackLinkProjection>,
    #[serde(default)]
    pub resolution: Option<ThreadResolution>,
    pub id: Uuid,
    pub position: String,
    pub revision: String,
    pub thread_id: Uuid,
    pub thread_version: String,
    pub thread_state: String,
    pub body: String,
    pub author_id: Uuid,
    pub created_at_ms: i64,
    pub anchor: Anchor,
    pub history: Vec<CommentRevision>,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultsPage {
    pub items: Vec<Detail>,
    pub next_cursor: Option<String>,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommentsPage {
    #[serde(default)]
    pub feedback_read_version: FeedbackReadVersion,
    #[serde(default)]
    pub follow_up_submission: FollowUpSubmission,
    pub items: Vec<CommentProjection>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedbackTaskStatus {
    pub operation_id: Uuid,
    pub task_id: Uuid,
    pub creation_disposition: crate::dashboard_api::actions::Disposition,
    pub link_confirmed: bool,
    pub result_id: Uuid,
    pub revision: String,
    pub origin: FeedbackDraft,
}

pub fn schema_bytes() -> Vec<u8> {
    #[allow(dead_code)]
    #[derive(JsonSchema)]
    #[serde(untagged)]
    enum Wire {
        Follow(Follow),
        Comment(Comment),
        Correction(Correction),
        Resolve(Resolve),
        Draft(FeedbackDraft),
        Link(FeedbackLink),
        Waiver(Waiver),
        Policy(CapturePolicy),
        FeedbackTaskStatus(crate::dashboard_api::contract::Envelope<FeedbackTaskStatus>),
        Detail(Detail),
        Results(crate::dashboard_api::contract::Envelope<ResultsPage>),
        Comments(crate::dashboard_api::contract::Envelope<CommentsPage>),
        Revision(crate::dashboard_api::contract::Envelope<Detail>),
    }
    let mut bytes =
        serde_json::to_vec_pretty(&schemars::schema_for!(Wire)).expect("static Results schema");
    bytes.push(b'\n');
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additive_reads_decode_legacy_and_current_results_feedback() {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../protocol/results-feedback/v1/fixtures/compatibility.json"
        ))
        .unwrap();
        let reads = &fixtures["additive_reads"];
        let old: Detail = serde_json::from_value(reads["legacy_detail"].clone()).unwrap();
        assert!(matches!(old.feedback_read_version, FeedbackReadVersion::V1));
        let current: Detail = serde_json::from_value(reads["detail_1_1"].clone()).unwrap();
        assert!(matches!(
            current.feedback_read_version,
            FeedbackReadVersion::V1_1
        ));
        assert!(matches!(
            current.follow_up_submission,
            FollowUpSubmission::Unsupported
        ));
        let old: CommentsPage = serde_json::from_value(reads["legacy_comments"].clone()).unwrap();
        assert!(matches!(old.feedback_read_version, FeedbackReadVersion::V1));
        assert!(old.items[0].feedback_links.is_empty());
        assert!(old.items[0].resolution.is_none());
        let current: CommentsPage = serde_json::from_value(reads["comments_1_1"].clone()).unwrap();
        assert_eq!(
            current.items[0].feedback_links[0].origin.comment_revision,
            "1"
        );
        assert_eq!(current.items[0].revision, "2");
        assert!(current.items[0].resolution.is_some());
    }
}
