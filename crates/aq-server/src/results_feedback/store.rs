use super::contract::*;
use crate::{
    AppState,
    dashboard_api::{
        actions::{Disposition, Outcome, existing},
        errors::ApiError,
    },
    store::User,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
pub(super) type Result<T> = std::result::Result<T, ApiError>;
pub(super) fn invalid() -> ApiError {
    ApiError::invalid()
}
fn conflict() -> ApiError {
    ApiError::for_status(StatusCode::CONFLICT)
}
pub(super) fn missing() -> ApiError {
    ApiError::for_status(StatusCode::NOT_FOUND)
}
pub(super) fn number(s: &str) -> Result<i64> {
    let n = s.parse::<i64>().map_err(|_| invalid())?;
    if n <= 0 || n.to_string() != s {
        return Err(invalid());
    }
    Ok(n)
}
pub(super) fn text(s: &str, max: usize) -> Result<()> {
    if s.trim().is_empty() || s.len() > max || s.contains('\0') {
        return Err(invalid());
    }
    Ok(())
}
#[derive(Clone)]
pub(super) struct Pin {
    pub report: Uuid,
    pub result: Uuid,
    pub revision: String,
    pub project: Uuid,
    pub task: Uuid,
    pub policy: CapturePolicy,
    pub submitted: i64,
}
impl Pin {
    fn decode(r: sqlx::postgres::PgRow) -> Result<Self> {
        Ok(Self {
            report: r.get("report_id"),
            result: r.get("result_id"),
            revision: r.get("report_revision"),
            project: r.get("project_id"),
            task: r.get("task_id"),
            policy: serde_json::from_value(r.get("capture_policy"))
                .map_err(|_| ApiError::unavailable())?,
            submitted: r.get("submitted_at_ms"),
        })
    }
}
pub(super) async fn pin(state: &AppState, id: Uuid, revision: Option<&str>) -> Result<Pin> {
    if id.is_nil() {
        return Err(invalid());
    }
    let r=sqlx::query("SELECT * FROM result_revision_pins WHERE (result_id=$1 OR report_id=$1) AND ($2::text IS NULL OR report_revision=$2) ORDER BY (report_id=$1) DESC,submitted_at_ms DESC,report_id DESC LIMIT 1")
        .bind(id).bind(revision).fetch_optional(&state.store.pool).await?.ok_or_else(missing)?;
    Pin::decode(r)
}
pub(super) async fn principal(state: &AppState, user: &User) -> Result<()> {
    existing::owner(state, user).await
}
async fn fence(tx: &mut Transaction<'_, Postgres>, user: &User) -> Result<()> {
    let r = sqlx::query("SELECT role,email,issuer,subject FROM users WHERE id=$1 FOR SHARE")
        .bind(user.id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(missing)?;
    if r.get::<String, _>("role") != "owner"
        || r.get::<String, _>("email") != user.email
        || r.get::<Option<String>, _>("issuer") != user.issuer
        || r.get::<Option<String>, _>("subject") != user.subject
    {
        return Err(ApiError::for_status(StatusCode::FORBIDDEN));
    }
    Ok(())
}
pub(super) async fn subject(state: &AppState, user: &User, id: Uuid, cmd: &Command) -> Result<Pin> {
    principal(state, user).await?;
    number(cmd.version())?;
    if cmd.id().is_nil() {
        return Err(invalid());
    }
    let p = match cmd {
        Command::Comment(v) => {
            if v.result_id != id {
                return Err(invalid());
            }
            pin(state, id, Some(&v.revision)).await?
        }
        Command::Draft(v) => {
            if v.result_id != id {
                return Err(invalid());
            }
            pin(state, id, Some(&v.revision)).await?
        }
        Command::Link(v) => {
            if v.result_id != id {
                return Err(invalid());
            }
            pin(state, id, Some(&v.revision)).await?
        }
        Command::Waiver(v) => pin(state, id, Some(&v.revision)).await?,
        Command::Follow(_) => pin(state, id, None).await?,
        Command::Correction(_) => {
            let report:Uuid=sqlx::query_scalar("SELECT t.report_id FROM result_comments c JOIN result_threads t ON t.id=c.thread_id WHERE c.id=$1").bind(id).fetch_optional(&state.store.pool).await?.ok_or_else(missing)?;
            pin(state, report, None).await?
        }
        Command::Resolve(_) => {
            let report: Uuid =
                sqlx::query_scalar("SELECT report_id FROM result_threads WHERE id=$1")
                    .bind(id)
                    .fetch_optional(&state.store.pool)
                    .await?
                    .ok_or_else(missing)?;
            pin(state, report, None).await?
        }
    };
    // Reuse the retained native report validator, including its current principal fence.
    crate::profiled_execution::human_results::read_as(
        state,
        user,
        &crate::profiled_execution::human_results::Page {
            profile: Some(crate::profiled_execution::human_results::ReadProfile::RecordedNativeV2),
            ..Default::default()
        },
        Some((p.report, &p.revision)),
    )
    .await?;
    match cmd {
        Command::Comment(v) => {
            text(&v.body, 16384)?;
            anchor(state, &p, &v.anchor).await?;
            if let Some(thread) = v.thread_id {
                let r =
                    sqlx::query("SELECT report_id,anchor,state FROM result_threads WHERE id=$1")
                        .bind(thread)
                        .fetch_optional(&state.store.pool)
                        .await?
                        .ok_or_else(missing)?;
                if r.get::<Uuid, _>("report_id") != p.report
                    || r.get::<Value, _>("anchor") != json!(v.anchor)
                    || r.get::<String, _>("state") != "open"
                {
                    return Err(invalid());
                }
            }
        }
        Command::Correction(v) => text(&v.body, 16384)?,
        Command::Resolve(v) => {
            text(&v.reason, 1024)?;
            let follow = pin(state, v.follow_up.result_id, Some(&v.follow_up.revision)).await?;
            if follow.project != p.project || follow.task != p.task {
                return Err(invalid());
            }
        }
        Command::Draft(v) => {
            text(&v.title, 800)?;
            if v.title.chars().count() > 200 || v.title.chars().any(char::is_control) {
                return Err(invalid());
            }
            text(&v.description, 32768)?;
            text(&v.acceptance_criteria, 8192)?;
            number(&v.expected_project_revision)?;
            origin(
                state,
                &p,
                v.thread_id,
                v.comment_id,
                &v.comment_revision,
                v.project_id,
                &v.artifacts,
            )
            .await?;
        }
        Command::Link(v) => {
            if v.task_id.is_nil() {
                return Err(invalid());
            }
            origin(
                state,
                &p,
                v.thread_id,
                v.comment_id,
                &v.comment_revision,
                v.project_id,
                &v.artifacts,
            )
            .await?;
        }
        Command::Waiver(v) => {
            text(&v.reason, 1024)?;
            if v.policy_revision != p.policy.revision
                || !p
                    .policy
                    .requirements
                    .iter()
                    .any(|r| r.id == v.requirement_id && r.policy_revision == v.policy_revision)
            {
                return Err(invalid());
            }
        }
        _ => {}
    }
    Ok(p)
}
async fn origin(
    state: &AppState,
    p: &Pin,
    thread: Uuid,
    comment: Uuid,
    revision: &str,
    project: Uuid,
    artifacts: &[ArtifactPin],
) -> Result<()> {
    if project != p.project || artifacts.len() > 20 {
        return Err(invalid());
    }
    let version = number(revision)?;
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM result_comments c JOIN result_threads t ON t.id=c.thread_id JOIN result_comment_revisions r ON r.comment_id=c.id WHERE c.id=$1 AND t.id=$2 AND t.report_id=$3 AND r.revision=$4)").bind(comment).bind(thread).bind(p.report).bind(version).fetch_one(&state.store.pool).await?;
    if !valid {
        return Err(invalid());
    }
    for a in artifacts {
        artifact(state, p, a.artifact_id, &a.digest).await?;
    }
    Ok(())
}
async fn artifact(state: &AppState, p: &Pin, id: Uuid, digest: &str) -> Result<Value> {
    let receipt:Vec<u8>=sqlx::query_scalar("SELECT receipt_bytes FROM result_media_intents WHERE operation_id=$1 AND report_id=$2 AND report_revision=$3 AND state='retained' AND cleanup_state='none' AND withheld_at_ms IS NULL AND retain_until_ms>$4")
        .bind(id).bind(p.report).bind(&p.revision).bind((state.clock)().timestamp_millis()).fetch_optional(&state.store.pool).await?.ok_or_else(invalid)?;
    let v: Value = serde_json::from_slice(&receipt).map_err(|_| ApiError::unavailable())?;
    if v["input"]["digest"] != digest && v["variants"]["render"]["digest"] != digest {
        return Err(invalid());
    }
    Ok(v)
}
fn unsupported_kind() -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "unsupported_media_kind",
        "The anchor kind does not match the cited artifact's media",
    )
}
async fn anchor(state: &AppState, p: &Pin, a: &Anchor) -> Result<()> {
    match a {
        Anchor::Result => Ok(()),
        Anchor::ImageRegion {
            artifact_id,
            digest,
            x,
            y,
            width,
            height,
        } => {
            if ![x, y, width, height]
                .iter()
                .all(|n| n.is_finite() && (0.0..=1.0).contains(*n))
                || *width <= 0.0
                || *height <= 0.0
                || x + width > 1.0
                || y + height > 1.0
            {
                return Err(invalid());
            }
            let receipt = artifact(state, p, *artifact_id, digest).await?;
            if receipt["rendering_policy"] != "aq.png.rgba8-srgb-v1" {
                return Err(unsupported_kind());
            }
            Ok(())
        }
        // A range cites the retained clip's receipt duration, never
        // client-supplied metadata; image artifacts have no timeline.
        Anchor::VideoRange {
            artifact_id,
            digest,
            start_ms,
            end_ms,
        } => {
            let receipt = artifact(state, p, *artifact_id, digest).await?;
            let Some(duration) = receipt["clip"]["duration_ms"].as_u64() else {
                return Err(unsupported_kind());
            };
            if receipt["rendering_policy"] != "aq.mp4.passthrough-v1"
                || start_ms >= end_ms
                || *end_ms > duration
            {
                return Err(invalid());
            }
            Ok(())
        }
    }
}
pub(super) async fn effect(state: &AppState, user: &User, id: Uuid) -> Result<Option<Outcome>> {
    principal(state, user).await?;
    let v: Option<Value> = sqlx::query_scalar(
        "SELECT outcome FROM result_feedback_effects WHERE operation_id=$1 AND actor_id=$2",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&state.store.pool)
    .await?;
    v.map(|v| {
        serde_json::from_value::<StoredOutcome>(v)
            .map(Into::into)
            .map_err(|_| ApiError::unavailable())
    })
    .transpose()
}
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredOutcome {
    disposition: Disposition,
    receipt_refs: Vec<crate::dashboard_api::actions::EntityRef>,
    affected_entities: Vec<crate::dashboard_api::actions::EntityRef>,
}
impl From<StoredOutcome> for Outcome {
    fn from(v: StoredOutcome) -> Self {
        Self {
            disposition: v.disposition,
            receipt_refs: v.receipt_refs,
            affected_entities: v.affected_entities,
            current_versions: Default::default(),
        }
    }
}
async fn retain(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    p: &Pin,
    id: Uuid,
    outcome: &Outcome,
) -> Result<()> {
    let v = json!({"disposition":outcome.disposition,"receipt_refs":outcome.receipt_refs,"affected_entities":outcome.affected_entities});
    sqlx::query("INSERT INTO result_feedback_effects VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(user.id)
        .bind(p.report)
        .bind(v)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
pub(super) async fn apply(
    state: &AppState,
    user: &User,
    id: Uuid,
    cmd: &Command,
) -> Result<Outcome> {
    let p = subject(state, user, id, cmd).await?;
    let mut tx = state.store.pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    let version: i64 =
        sqlx::query_scalar("SELECT version FROM result_aggregates WHERE result_id=$1 FOR UPDATE")
            .bind(p.result)
            .fetch_one(&mut *tx)
            .await?;
    fence(&mut tx, user).await?;
    let expected = number(cmd.version())?;
    let now = (state.clock)().timestamp_millis();
    let receipt = existing::entity("result_feedback_receipt", cmd.id(), Some("1".into()));
    let mut entities = vec![existing::entity(
        "result",
        p.result,
        Some((version + 1).to_string()),
    )];
    match cmd {
        Command::Follow(v) => {
            if version != expected {
                return Err(conflict());
            }
            sqlx::query("INSERT INTO result_follows VALUES($1,$2,$3) ON CONFLICT(result_id,actor_id) DO UPDATE SET following=excluded.following").bind(p.result).bind(user.id).bind(v.following).execute(&mut *tx).await?;
        }
        Command::Comment(v) => {
            if version != expected {
                return Err(conflict());
            }
            let thread = v.thread_id.unwrap_or(v.operation_id);
            if v.thread_id.is_none() {
                sqlx::query("INSERT INTO result_threads(id,report_id,report_revision,anchor) VALUES($1,$2,$3,$4)").bind(thread).bind(p.report).bind(&p.revision).bind(json!(v.anchor)).execute(&mut *tx).await?;
            } else {
                let updated=sqlx::query("UPDATE result_threads SET version=version+1 WHERE id=$1 AND state='open' AND report_id=$2 AND anchor=$3").bind(thread).bind(p.report).bind(json!(v.anchor)).execute(&mut *tx).await?;
                if updated.rows_affected() != 1 {
                    return Err(conflict());
                }
            }
            sqlx::query("INSERT INTO result_comments(id,thread_id,created_at_ms) VALUES($1,$2,$3)")
                .bind(v.operation_id)
                .bind(thread)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO result_comment_revisions VALUES($1,1,$2,$3,$4)")
                .bind(v.operation_id)
                .bind(user.id)
                .bind(&v.body)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO result_thread_follows VALUES($1,$2,$3) ON CONFLICT(thread_id,actor_id) DO UPDATE SET following=result_thread_follows.following OR excluded.following").bind(thread).bind(user.id).bind(v.follow).execute(&mut *tx).await?;
            entities.push(existing::entity(
                "comment",
                v.operation_id,
                Some("1".into()),
            ));
            entities.push(existing::entity("thread", thread, None));
        }
        Command::Correction(v) => {
            let updated=sqlx::query("UPDATE result_comments SET revision=revision+1 WHERE id=$1 AND revision=$2 AND revision<16").bind(id).bind(expected).execute(&mut *tx).await?;
            if updated.rows_affected() != 1 {
                return Err(conflict());
            }
            sqlx::query("INSERT INTO result_comment_revisions VALUES($1,$2,$3,$4,$5)")
                .bind(id)
                .bind(expected + 1)
                .bind(user.id)
                .bind(&v.body)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            entities.push(existing::entity(
                "comment",
                id,
                Some((expected + 1).to_string()),
            ));
        }
        Command::Resolve(v) => {
            let follow = pin(state, v.follow_up.result_id, Some(&v.follow_up.revision)).await?;
            let updated=sqlx::query("UPDATE result_threads SET version=version+1,state='resolved',resolved_by=$3,reason=$4,follow_up_report=$5,follow_up_revision=$6 WHERE id=$1 AND version=$2 AND state='open'").bind(id).bind(expected).bind(user.id).bind(&v.reason).bind(follow.report).bind(&follow.revision).execute(&mut *tx).await?;
            if updated.rows_affected() != 1 {
                return Err(conflict());
            }
            entities.push(existing::entity(
                "thread",
                id,
                Some((expected + 1).to_string()),
            ));
        }
        Command::Waiver(v) => {
            // Evidence decisions guard their own assessment version.
            let av: i64 = sqlx::query_scalar(
                "SELECT version FROM result_assessments WHERE report_id=$1 FOR UPDATE",
            )
            .bind(p.report)
            .fetch_one(&mut *tx)
            .await?;
            if av != expected {
                return Err(conflict());
            }
            let inserted=sqlx::query("INSERT INTO result_evidence_waivers VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(report_id,requirement_id,policy_revision) DO NOTHING")
                .bind(v.operation_id)
                .bind(p.report)
                .bind(&p.revision)
                .bind(&v.requirement_id)
                .bind(&v.policy_revision)
                .bind(user.id)
                .bind(&v.reason)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            if inserted.rows_affected() != 1 {
                return Err(conflict());
            }
        }
        Command::Link(v) => {
            guard_thread(&mut tx, v.thread_id, expected).await?;
            link(&mut tx, &p, v.operation_id, v.task_id, json!(v)).await?;
            entities.push(existing::entity("task", v.task_id, None));
        }
        Command::Draft(_) => return Err(invalid()),
    }
    sqlx::query("UPDATE result_aggregates SET version=version+1 WHERE result_id=$1")
        .bind(p.result)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE result_assessments SET version=version+1,observed_state=NULL WHERE report_id=$1",
    )
    .bind(p.report)
    .execute(&mut *tx)
    .await?;
    let out = Outcome::confirmed(vec![receipt], entities);
    retain(&mut tx, user, &p, cmd.id(), &out).await?;
    tx.commit().await?;
    Ok(out)
}
async fn link(
    tx: &mut Transaction<'_, Postgres>,
    p: &Pin,
    id: Uuid,
    task: Uuid,
    origin: Value,
) -> Result<()> {
    sqlx::query("INSERT INTO result_feedback_links VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(operation_id) DO NOTHING").bind(id).bind(p.report).bind(&p.revision).bind(task).bind(p.project).bind(origin).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn detail(state: &AppState, user: &User, p: &Pin) -> Result<Detail> {
    let page = crate::profiled_execution::human_results::read_as(
        state,
        user,
        &crate::profiled_execution::human_results::Page {
            profile: Some(crate::profiled_execution::human_results::ReadProfile::RecordedNativeV2),
            ..Default::default()
        },
        Some((p.report, &p.revision)),
    )
    .await?;
    let m = page.items.into_iter().next().ok_or_else(missing)?;
    let version: i64 =
        sqlx::query_scalar("SELECT version FROM result_aggregates WHERE result_id=$1")
            .bind(p.result)
            .fetch_one(&state.store.pool)
            .await?;
    let current: String=sqlx::query_scalar("SELECT p.report_revision FROM result_revision_pins p WHERE p.result_id=$1 AND NOT EXISTS(SELECT 1 FROM profiled_result_reports next WHERE next.supersedes=p.report_id)").bind(p.result).fetch_one(&state.store.pool).await?;
    let directly_following: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT following FROM result_follows WHERE result_id=$1 AND actor_id=$2),false)",
    )
    .bind(p.result)
    .bind(user.id)
    .fetch_one(&state.store.pool)
    .await?;
    let addressed_feedback: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM result_threads WHERE state='resolved' AND follow_up_report=$1 AND follow_up_revision=$2 ORDER BY id",
    )
    .bind(p.report)
    .bind(&p.revision)
    .fetch_all(&state.store.pool)
    .await?;
    let now = (state.clock)().timestamp_millis();
    let artifacts=sqlx::query("SELECT operation_id,input_digest,capture,rendering_policy FROM result_media_intents WHERE report_id=$1 AND report_revision=$2 AND state='retained' AND cleanup_state='none' AND withheld_at_ms IS NULL AND retain_until_ms>$3 ORDER BY operation_id").bind(p.report).bind(&p.revision).bind(now).fetch_all(&state.store.pool).await?;
    let mut requirements = Vec::new();
    for r in &p.policy.requirements {
        let waiver:Option<Uuid>=sqlx::query_scalar("SELECT operation_id FROM result_evidence_waivers WHERE report_id=$1 AND report_revision=$2 AND requirement_id=$3 AND policy_revision=$4").bind(p.report).bind(&p.revision).bind(&r.id).bind(&r.policy_revision).fetch_optional(&state.store.pool).await?;
        let ids: Vec<Uuid> = artifacts
            .iter()
            .filter(|a| {
                let c: Value = a.get("capture");
                let policy: &str = a.get("rendering_policy");
                matches!(
                    (r.kind.as_str(), policy),
                    ("image", "aq.png.rgba8-srgb-v1") | ("video", "aq.mp4.passthrough-v1")
                ) && c["scenario"] == r.scenario
                    && c["environment"] == r.environment
                    && c["viewport"] == json!(r.viewport)
                    && c["claimed_source"] == json!(m.source_base)
            })
            .map(|a| a.get("operation_id"))
            .collect();
        let state = if waiver.is_some() {
            "waived"
        } else if ids.len() >= usize::from(r.count) {
            "retained"
        } else if r.required {
            "missing"
        } else {
            "optional"
        };
        requirements.push(RequirementEvaluation {
            requirement_id: r.id.clone(),
            policy_revision: r.policy_revision.clone(),
            required: r.required,
            state: state.into(),
            artifact_ids: ids,
            waiver_id: waiver,
        });
    }
    let evidence = Evaluation {
        satisfied: requirements
            .iter()
            .all(|r| !r.required || matches!(r.state.as_str(), "waived" | "retained")),
        requirements,
    };
    let unresolved:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM result_threads t JOIN result_revision_pins p USING(report_id) WHERE p.result_id=(SELECT result_id FROM result_revision_pins WHERE report_id=$1) AND t.state='open') OR EXISTS(SELECT 1 FROM review_subjects s JOIN human_decision_requests d USING(subject_digest) WHERE s.report_id=$1 AND d.state='pending' AND d.expires_at_ms>$2)").bind(p.report).bind(now).fetch_one(&state.store.pool).await?;
    let observed = json!({"evidence":evidence,"unresolved_review":unresolved,"addressed_feedback":addressed_feedback,"artifacts":artifacts.iter().map(|a|a.get::<Uuid,_>("operation_id")).collect::<Vec<_>>()});
    let av:i64=sqlx::query_scalar("UPDATE result_assessments SET version=version+CASE WHEN observed_state IS NOT NULL AND observed_state IS DISTINCT FROM $2 THEN 1 ELSE 0 END,observed_state=$2 WHERE report_id=$1 RETURNING version").bind(p.report).bind(observed).fetch_one(&state.store.pool).await?;
    let parent_results = if let Some(parent) = m.supersedes {
        let old = pin(state, parent, None).await?;
        vec![RevisionRef {
            result_id: old.result,
            revision: old.revision,
        }]
    } else {
        vec![]
    };
    Ok(Detail {
        feedback_read_version: FeedbackReadVersion::V1_1,
        follow_up_submission: FollowUpSubmission::Unsupported,
        result: ResultProjection {
            directly_following,
            id: p.result,
            version: version.to_string(),
            project_id: p.project,
            task_id: p.task,
            current_revision: current,
        },
        revision: ResultRevision {
            report_id: p.report,
            revision: p.revision.clone(),
            attempt_id: m.attempt_id,
            source: Source {
                repository_id: m.repository_id,
                base: m.source_base,
            },
            summary: m.conclusion,
            submitted_at_ms: p.submitted as u64,
            parent_results,
            capture_policy: p.policy.clone(),
            artifacts: vec![],
            check_refs: vec![],
            evidence_refs: m.source_receipt_ids,
            addressed_feedback,
        },
        assessment: ResultAssessment {
            version: av.to_string(),
            result_revision: p.revision.clone(),
            evidence,
            artifacts: artifacts
                .iter()
                .map(|a| ArtifactPin {
                    artifact_id: a.get("operation_id"),
                    digest: a.get("input_digest"),
                })
                .collect(),
            acceptance: "unreviewed".into(),
            observed_at_ms: now as u64,
            unresolved_review: unresolved,
        },
    })
}

pub(super) async fn comments(
    state: &AppState,
    user: &User,
    p: &Pin,
    after: i64,
    snapshot: i64,
    limit: i64,
) -> Result<Vec<Value>> {
    principal(state, user).await?;
    let rows=sqlx::query("SELECT c.id,c.revision,c.created_at_ms,c.position,t.id AS thread_id,t.version AS thread_version,t.state,t.anchor,t.resolved_by,t.reason,t.follow_up_report,t.follow_up_revision,fp.result_id AS follow_up_result_id,r.body,r.author_id FROM result_comments c JOIN result_threads t ON t.id=c.thread_id JOIN result_comment_revisions r ON r.comment_id=c.id AND r.revision=c.revision LEFT JOIN result_revision_pins fp ON fp.report_id=t.follow_up_report AND fp.report_revision=t.follow_up_revision WHERE t.report_id=$1 AND t.report_revision=$2 AND c.position<=$3 AND c.position>$4 ORDER BY c.position LIMIT $5").bind(p.report).bind(&p.revision).bind(snapshot).bind(after).bind(limit).fetch_all(&state.store.pool).await?;
    let mut output = Vec::new();
    for r in rows {
        let id: Uuid = r.get("id");
        let history:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('revision',revision::text,'body',body,'author_id',author_id,'recorded_at_ms',recorded_at_ms) FROM result_comment_revisions WHERE comment_id=$1 ORDER BY revision").bind(id).fetch_all(&state.store.pool).await?;
        let links = sqlx::query("SELECT operation_id,project_id,task_id,origin FROM result_feedback_links WHERE report_id=$1 AND report_revision=$2 AND origin->>'comment_id'=$3 AND origin->>'thread_id'=$4 ORDER BY operation_id")
            .bind(p.report).bind(&p.revision).bind(id.to_string())
            .bind(r.get::<Uuid, _>("thread_id").to_string())
            .fetch_all(&state.store.pool).await?;
        let mut feedback_links = Vec::new();
        for link in links {
            let origin: Value = link.get("origin");
            let origin: FeedbackOrigin = serde_json::from_value(json!({
                "result_id":origin["result_id"], "revision":origin["revision"],
                "thread_id":origin["thread_id"], "comment_id":origin["comment_id"],
                "comment_revision":origin["comment_revision"], "artifacts":origin["artifacts"],
            }))
            .map_err(|_| ApiError::unavailable())?;
            feedback_links.push(FeedbackLinkProjection {
                operation_id: link.get("operation_id"),
                project_id: link.get("project_id"),
                task_id: link.get("task_id"),
                origin,
            });
        }
        let resolution = if r.get::<String, _>("state") == "resolved" {
            Some(ThreadResolution {
                resolved_by: r
                    .try_get("resolved_by")
                    .map_err(|_| ApiError::unavailable())?,
                reason: r.try_get("reason").map_err(|_| ApiError::unavailable())?,
                follow_up: RevisionRef {
                    result_id: r
                        .try_get("follow_up_result_id")
                        .map_err(|_| ApiError::unavailable())?,
                    revision: r
                        .try_get("follow_up_revision")
                        .map_err(|_| ApiError::unavailable())?,
                },
                follow_up_report_id: r
                    .try_get("follow_up_report")
                    .map_err(|_| ApiError::unavailable())?,
            })
        } else {
            None
        };
        output.push(json!({"feedback_links":feedback_links,"resolution":resolution,"position":r.get::<i64,_>("position").to_string(),"id":id,"revision":r.get::<i64,_>("revision").to_string(),"thread_id":r.get::<Uuid,_>("thread_id"),"thread_version":r.get::<i64,_>("thread_version").to_string(),"thread_state":r.get::<String,_>("state"),"body":r.get::<String,_>("body"),"author_id":r.get::<Uuid,_>("author_id"),"created_at_ms":r.get::<i64,_>("created_at_ms"),"anchor":r.get::<Value,_>("anchor"),"history":history}));
    }
    principal(state, user).await?;
    Ok(output)
}

pub(super) async fn draft_intent(
    state: &AppState,
    user: &User,
    p: &Pin,
    v: &FeedbackDraft,
) -> Result<()> {
    let mut tx = state.store.pool.begin().await?;
    sqlx::query("SELECT version FROM result_aggregates WHERE result_id=$1 FOR UPDATE")
        .bind(p.result)
        .fetch_one(&mut *tx)
        .await?;
    fence(&mut tx, user).await?;
    guard_thread(&mut tx, v.thread_id, number(&v.expected_version)?).await?;
    sqlx::query("INSERT INTO result_feedback_intents VALUES($1,$2,$3,$4,$5) ON CONFLICT(operation_id) DO NOTHING").bind(v.operation_id).bind(user.id).bind(p.report).bind(&p.revision).bind(json!(v)).execute(&mut *tx).await?;
    let old: Value = sqlx::query_scalar(
        "SELECT request FROM result_feedback_intents WHERE operation_id=$1 AND actor_id=$2",
    )
    .bind(v.operation_id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    if old != json!(v) {
        return Err(conflict());
    }
    tx.commit().await?;
    Ok(())
}
pub(super) async fn finish_draft(
    state: &AppState,
    user: &User,
    p: &Pin,
    v: &FeedbackDraft,
    native: Outcome,
) -> Result<Outcome> {
    if native.disposition != Disposition::Confirmed {
        return Ok(native);
    }
    if native.affected_entities.len() != 1
        || native.affected_entities[0].id != v.operation_id.to_string()
    {
        return Err(ApiError::unavailable());
    }
    let mut tx = state.store.pool.begin().await?;
    sqlx::query("SELECT version FROM result_aggregates WHERE result_id=$1 FOR UPDATE")
        .bind(p.result)
        .fetch_one(&mut *tx)
        .await?;
    fence(&mut tx, user).await?;
    if let Some(value) = sqlx::query_scalar::<_, Value>(
        "SELECT outcome FROM result_feedback_effects WHERE operation_id=$1 AND actor_id=$2",
    )
    .bind(v.operation_id)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?
    {
        return serde_json::from_value::<StoredOutcome>(value)
            .map(Into::into)
            .map_err(|_| ApiError::unavailable());
    }
    link(&mut tx, p, v.operation_id, v.operation_id, json!(v)).await?;
    let mut outcome = native;
    outcome.receipt_refs.push(existing::entity(
        "feedback_link",
        v.operation_id,
        Some("1".into()),
    ));
    retain(&mut tx, user, p, v.operation_id, &outcome).await?;
    sqlx::query("UPDATE result_aggregates SET version=version+1 WHERE result_id=$1")
        .bind(p.result)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(outcome)
}
pub(super) async fn retained_draft(
    state: &AppState,
    user: &User,
    id: Uuid,
) -> Result<Option<FeedbackDraft>> {
    principal(state, user).await?;
    let raw: Option<Value> = sqlx::query_scalar(
        "SELECT request FROM result_feedback_intents WHERE operation_id=$1 AND actor_id=$2",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&state.store.pool)
    .await?;
    raw.map(|v| serde_json::from_value(v).map_err(|_| ApiError::unavailable()))
        .transpose()
}

async fn guard_thread(
    tx: &mut Transaction<'_, Postgres>,
    thread: Uuid,
    expected: i64,
) -> Result<()> {
    let version: i64 =
        sqlx::query_scalar("SELECT version FROM result_threads WHERE id=$1 FOR UPDATE")
            .bind(thread)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(missing)?;
    if version != expected {
        return Err(conflict());
    }
    Ok(())
}
/// Freeze bounded feed membership before issuing a G1 cursor. Every returned
/// item is subsequently revalidated through the native retained-report owner.
#[allow(clippy::too_many_arguments)]
pub(super) async fn feed(
    state: &AppState,
    user: &User,
    mode: &str,
    project: Option<Uuid>,
    task: Option<Uuid>,
    media: &str,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<Vec<(Uuid, String)>> {
    principal(state, user).await?;
    let now = (state.clock)().timestamp_millis();
    let rows=sqlx::query("SELECT p.* FROM result_revision_pins p WHERE NOT EXISTS(SELECT 1 FROM profiled_result_reports newer WHERE newer.supersedes=p.report_id) AND ($1::uuid IS NULL OR p.project_id=$1) AND ($2::uuid IS NULL OR p.task_id=$2) AND ($3::bigint IS NULL OR p.submitted_at_ms>=$3) AND ($4::bigint IS NULL OR p.submitted_at_ms<=$4) AND ($5='any' OR ($5<>'none')=EXISTS(SELECT 1 FROM result_media_intents m WHERE m.report_id=p.report_id AND m.report_revision=p.report_revision AND m.state='retained' AND m.cleanup_state='none' AND m.withheld_at_ms IS NULL AND m.retain_until_ms>$6 AND ($5='none' OR m.rendering_policy=CASE $5 WHEN 'video' THEN 'aq.mp4.passthrough-v1' ELSE 'aq.png.rgba8-srgb-v1' END))) AND ($7<>'following' OR EXISTS(SELECT 1 FROM result_follows f WHERE f.result_id=p.result_id AND f.actor_id=$8 AND f.following) OR EXISTS(SELECT 1 FROM result_thread_follows f JOIN result_threads t ON t.id=f.thread_id JOIN result_revision_pins old ON old.report_id=t.report_id WHERE old.result_id=p.result_id AND f.actor_id=$8 AND f.following)) ORDER BY p.submitted_at_ms DESC,p.report_id DESC LIMIT 1001")
 .bind(project).bind(task).bind(since).bind(until).bind(media).bind(now).bind(mode).bind(user.id).fetch_all(&state.store.pool).await?;
    if rows.len() > 1000 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "query_scope_too_large",
            "Narrow the project, task or time filters",
        ));
    }
    let mut pins = Vec::new();
    for row in rows {
        let p = Pin::decode(row)?;
        if mode == "needs_review" {
            let d = detail(state, user, &p).await?;
            if !d.assessment.unresolved_review && d.assessment.evidence.satisfied {
                continue;
            }
        }
        pins.push((p.report, p.revision));
    }
    Ok(pins)
}
pub(super) async fn effect_pin(state: &AppState, user: &User, id: Uuid) -> Result<Option<Pin>> {
    principal(state, user).await?;
    let report: Option<Uuid> = sqlx::query_scalar(
        "SELECT report_id FROM result_feedback_effects WHERE operation_id=$1 AND actor_id=$2",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&state.store.pool)
    .await?;
    match report {
        Some(id) => pin(state, id, None).await.map(Some),
        None => Ok(None),
    }
}
