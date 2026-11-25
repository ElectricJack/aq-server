//! Human review decision and inbox projections.
use aq_review_protocol::{
    ArtifactRef, Choice, DecisionFinality, DecisionRequest, ReviewSubject, Text,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

/// Decision kinds served by this owner. Question decisions (F5) add a variant.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    #[default]
    Review,
}
/// Kind-specific payload for `review` decisions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewPayload {
    #[schemars(regex(pattern = "^code_review_v1$"))]
    pub subject_profile: String,
    pub task_version: u64,
    pub task_content_revision: u64,
    pub criteria_count: u32,
    #[schemars(regex(pattern = "^(success|failed|cancelled|expired|unknown)$"))]
    pub process_outcome: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestedBy {
    pub user_id: Uuid,
    #[schemars(length(max = 320))]
    pub email: String,
}
/// Direct downstream reach: distinct live tasks this task `blocks`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Impact {
    pub blocked_tasks: u32,
    /// The bounded graph read was truncated; the count is a lower bound.
    pub truncated: bool,
}
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InboxOrder {
    /// Stable decision-id order with cursor continuation.
    #[default]
    Decision,
    /// Downstream impact desc, then not-deferred, then oldest submitted.
    Impact,
}

#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxQuery {
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub state: InboxState,
    /// `impact` requires `state=open`.
    #[serde(default)]
    pub order: InboxOrder,
    #[serde(default = "page_size")]
    pub limit: u32,
    pub cursor: Option<String>,
}
fn page_size() -> u32 {
    25
}
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InboxState {
    #[default]
    Open,
    Closed,
}
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxItem {
    pub decision_id: Uuid,
    pub decision_revision: u64,
    #[schemars(regex(pattern = "^(pending|answered|expired|cancelled)$"))]
    pub state: String,
    pub expires_at_ms: u64,
    pub project_id: Uuid,
    pub task_id: Uuid,
    pub report_id: Uuid,
    pub current_report_head: bool,
    pub eligible_owner: bool,
    pub kind: DecisionKind,
    /// Graph task title; null when the graph is unavailable to this read.
    #[schemars(length(max = 4096))]
    pub title: Option<String>,
    #[schemars(length(max = 1024))]
    pub requested_judgment: String,
    pub kind_payload: ReviewPayload,
    pub requested_by: RequestedBy,
    /// Null for decisions admitted before submission times were recorded.
    pub submitted_at_ms: Option<u64>,
    /// Null when the graph is unavailable to this read.
    pub impact: Option<Impact>,
    /// This owner's active deferral, if any.
    pub deferred_until_ms: Option<u64>,
}
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboxPage {
    #[schemars(regex(pattern = "^review-inbox-1$"))]
    pub api_version: String,
    pub server_time_ms: u64,
    #[schemars(length(max = 50))]
    pub items: Vec<InboxItem>,
    #[schemars(length(max = 4096))]
    pub next_cursor: Option<String>,
    /// Pages sample current authorized rows; they are not a transactional snapshot.
    pub sampled: bool,
    /// Pending decisions requested by or awaiting this owner (in the project
    /// filter), or null when more than the bounded scan exists.
    pub open_total: Option<u64>,
    pub order: InboxOrder,
    /// False when impact ordering ran on partial data (scan bound reached or
    /// some impacts unavailable); such items keep submitted-time order.
    pub order_complete: bool,
}

const TITLE_CHARS: usize = 120;
pub fn judgment(title: Option<&str>, task: Uuid, criteria: usize) -> String {
    let what = match title {
        Some(t) => {
            let mut t: String = t.chars().take(TITLE_CHARS).collect();
            if t.chars().count() < title.unwrap_or_default().chars().count() {
                t.push('…');
            }
            format!("\"{t}\"")
        }
        None => format!("task {task}"),
    };
    let noun = if criteria == 1 {
        "criterion"
    } else {
        "criteria"
    };
    format!(
        "Review the recorded result of {what} against its {criteria} acceptance {noun} and the pinned source change, then approve it or request changes."
    )
}
/// Every pinned evidence reference with its dashboard/API link when one exists.
pub fn evidence_refs(s: &ReviewSubject) -> Vec<(&'static str, &ArtifactRef, serde_json::Value)> {
    let (report, digest) = (s.result.report_id, s.result.report_digest.as_str());
    let result = json!({"href":format!("/results/{report}/revisions/{digest}"),"api":format!("/api/human/results/{report}/revisions/{digest}")});
    let (project, task) = (s.project_id, s.task.task_id);
    let work = json!({"href":format!("/work/{task}"),"api":format!("/api/human/projects/{project}/tasks/{task}")});
    let none = serde_json::Value::Null;
    vec![
        ("result_record", &s.result.record_receipt, result.clone()),
        ("stopped_receipt", &s.result.stopped_receipt, result),
        ("task_observation", &s.task_observation, work.clone()),
        ("task_context", &s.task_context, work),
        ("execution_spec", &s.execution_spec, none.clone()),
        ("expected_outcome", &s.expected_outcome, none.clone()),
        ("validation_policy", &s.validation_policy, none.clone()),
        ("source_input", &s.source.input_artifact, none.clone()),
        ("source_result", &s.source.resulting_artifact, none.clone()),
        ("source_retention", &s.source.retention_receipt, none),
    ]
}
pub fn changes(from: &ReviewSubject, to: &ReviewSubject) -> Vec<serde_json::Value> {
    let pairs = [
        (
            "original_project_revision",
            from.original_project_revision.to_string(),
            to.original_project_revision.to_string(),
        ),
        (
            "task.version",
            from.task.version.to_string(),
            to.task.version.to_string(),
        ),
        (
            "task.content_revision",
            from.task.content_revision.to_string(),
            to.task.content_revision.to_string(),
        ),
        (
            "result.report_id",
            from.result.report_id.to_string(),
            to.result.report_id.to_string(),
        ),
        (
            "source.base",
            from.source.base.hex.clone(),
            to.source.base.hex.clone(),
        ),
        (
            "source.resulting_object",
            from.source.resulting_object.hex.clone(),
            to.source.resulting_object.hex.clone(),
        ),
        ("criteria", criteria(&from.criteria), criteria(&to.criteria)),
    ];
    pairs
        .into_iter()
        .filter(|(_, a, b)| a != b)
        .map(|(field, a, b)| json!({"field":field,"from":a,"to":b}))
        .collect()
}
fn criteria(c: &[Text]) -> String {
    c.iter().map(Text::as_str).collect::<Vec<_>>().join(",")
}
pub fn choices(open: bool) -> serde_json::Value {
    json!([
        {"id":"approve","choice":"approve","route":"resolve","final":true,"available":open,"label":"Approve",
         "next":"Records your approval as the final answer. The workflow that requested this review receives the approved outcome and continues."},
        {"id":"request_changes","choice":"reject","route":"resolve","final":true,"available":open,"label":"Request changes",
         "next":"Records a final rejection. The workflow that requested this review receives the rejected outcome; a later result needs a new decision."},
        {"id":"defer","choice":null,"route":"defer","final":false,"available":open,"label":"Defer",
         "next":"Records nothing on the decision. It stays pending, ranks below undeferred decisions in your impact-ordered inbox until the chosen time, and still expires on schedule."}
    ])
}
pub fn next_text(state: &str, expires: u64, deferred: Option<u64>) -> String {
    match state {
        "pending" => match deferred {
            Some(until) => format!("Pending; you deferred it until {until} (ms since epoch). An eligible owner can still answer it; unanswered, it expires at {expires}."),
            None => format!("Waiting for an eligible owner to approve or request changes; unanswered, it expires at {expires} (ms since epoch)."),
        },
        "answered" => "Answered. The outcome has been delivered to the workflow that requested the review.".into(),
        "expired" => "Expired without an answer. The requesting workflow receives the expired outcome.".into(),
        _ => "Cancelled. The requesting workflow receives the cancelled outcome; no answer can be recorded.".into(),
    }
}

/// Plaintext row metadata for one decision as seen by `actor`.
pub struct ReviewMetadata {
    pub requested_by: RequestedBy,
    pub submitted_at_ms: Option<u64>,
    pub deferred_until_ms: Option<u64>,
}
/// Fields shared by inbox items and the decision view.
pub struct Enrichment {
    pub kind: DecisionKind,
    pub title: Option<String>,
    pub requested_judgment: String,
    pub kind_payload: ReviewPayload,
    pub requested_by: RequestedBy,
    pub submitted_at_ms: Option<u64>,
    pub impact: Option<Impact>,
    pub deferred_until_ms: Option<u64>,
}
impl ReviewMetadata {
    pub fn enrich(
        self,
        subject: &ReviewSubject,
        view: &serde_json::Value,
        brief: (Option<String>, Option<Impact>),
    ) -> Result<Enrichment, std::num::TryFromIntError> {
        let (title, impact) = brief;
        Ok(Enrichment {
            kind: DecisionKind::Review,
            requested_judgment: judgment(
                title.as_deref(),
                subject.task.task_id,
                subject.criteria.len(),
            ),
            title,
            kind_payload: ReviewPayload {
                subject_profile: "code_review_v1".into(),
                task_version: subject.task.version,
                task_content_revision: subject.task.content_revision,
                criteria_count: u32::try_from(subject.criteria.len())?,
                process_outcome: view["recorded_result"]["process_outcome"]
                    .as_str()
                    .map(str::to_owned),
            },
            requested_by: self.requested_by,
            submitted_at_ms: self.submitted_at_ms,
            impact,
            deferred_until_ms: self.deferred_until_ms,
        })
    }
}
impl Enrichment {
    pub fn append_fields(&self, o: &mut serde_json::Map<String, serde_json::Value>) {
        o.insert("kind".into(), json!(self.kind));
        o.insert("title".into(), json!(self.title));
        o.insert("requested_judgment".into(), json!(self.requested_judgment));
        o.insert("kind_payload".into(), json!(self.kind_payload));
        o.insert("requested_by".into(), json!(self.requested_by));
        o.insert("submitted_at_ms".into(), json!(self.submitted_at_ms));
        o.insert("impact".into(), json!(self.impact));
        o.insert("deferred_until_ms".into(), json!(self.deferred_until_ms));
    }
}

pub struct ReviewDetail<'a> {
    view: &'a serde_json::Value,
    request: DecisionRequest,
    subject: ReviewSubject,
}
impl<'a> ReviewDetail<'a> {
    pub fn read(view: &'a serde_json::Value) -> Result<Self, serde_json::Error> {
        let request = serde_json::from_value(view["request"].clone())?;
        let subject = serde_json::from_value(view["subject"].clone())?;
        Ok(Self {
            view,
            request,
            subject,
        })
    }
    pub fn inbox_item(self, id: Uuid, reader: Uuid, e: Enrichment) -> Option<InboxItem> {
        let Self {
            view,
            request,
            subject,
        } = self;
        Some(InboxItem {
            decision_id: id,
            decision_revision: view["decision_revision"].as_u64()?,
            state: view["state"].as_str()?.into(),
            expires_at_ms: request.expires_at_ms,
            project_id: subject.project_id,
            task_id: subject.task.task_id,
            report_id: subject.result.report_id,
            current_report_head: view["current_report_head"].as_bool()?,
            eligible_owner: request.eligible_owners.contains(&reader),
            kind: e.kind,
            title: e.title,
            requested_judgment: e.requested_judgment,
            kind_payload: e.kind_payload,
            requested_by: e.requested_by,
            submitted_at_ms: e.submitted_at_ms,
            impact: e.impact,
            deferred_until_ms: e.deferred_until_ms,
        })
    }
}

pub fn response_outcome(finality: &DecisionFinality) -> (&'static str, Option<Uuid>, bool) {
    match finality {
        DecisionFinality::HumanAnswer {
            owner_user_id,
            choice,
            comment_digest,
            ..
        } => (
            match choice {
                Choice::Approve => "approve",
                Choice::Reject => "reject",
            },
            Some(*owner_user_id),
            comment_digest.is_some(),
        ),
        DecisionFinality::Expired {} => ("expired", None, false),
        DecisionFinality::Cancelled { .. } => ("cancelled", None, false),
    }
}
pub fn order_responses(out: &mut [(serde_json::Value, ReviewSubject)]) {
    out.sort_by(|(a, _), (b, _)| {
        b["recorded_at_ms"]
            .as_u64()
            .cmp(&a["recorded_at_ms"].as_u64())
            .then_with(|| a["decision_id"].as_str().cmp(&b["decision_id"].as_str()))
    });
}

/// One authorized pending row from the bounded open scan.
pub struct OpenReview {
    pub id: Uuid,
    pub project: Uuid,
    pub task: Uuid,
    pub submitted: Option<u64>,
    pub deferred: bool,
}
/// Order the bounded impact sample for display and cursor continuation.
pub fn order_by_impact(ranked: &mut [(OpenReview, Option<u32>)]) {
    ranked.sort_by(|(a, ra), (b, rb)| {
        rb.cmp(ra)
            .then(a.deferred.cmp(&b.deferred))
            .then(a.submitted.is_none().cmp(&b.submitted.is_none()))
            .then(a.submitted.cmp(&b.submitted))
            .then(a.id.cmp(&b.id))
    });
}
#[cfg(test)]
mod tests {
    use super::judgment;
    use uuid::Uuid;
    #[test]
    fn judgment_text_names_title_or_task_and_truncates() {
        let t = Uuid::nil();
        assert_eq!(
            judgment(Some("Ship it"), t, 1),
            "Review the recorded result of \"Ship it\" against its 1 acceptance criterion and the pinned source change, then approve it or request changes."
        );
        assert!(
            judgment(None, t, 3).contains(&format!("task {t} against its 3 acceptance criteria"))
        );
        let long = "x".repeat(200);
        assert!(judgment(Some(&long), t, 2).contains(&format!("\"{}…\"", "x".repeat(120))));
    }
}
