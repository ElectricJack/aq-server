//! Human decision view contracts (G11 X14–X19): decision kind and payload,
//! title and requested judgment, requester and submission time, downstream
//! impact, previous responses with the change since the previous revision,
//! choices (including the non-final per-owner Defer) and per-evidence viewed
//! state with resolvable links. Graph enrichment is best-effort and bounded:
//! an unavailable graph yields null title/impact, never a failed decision read.
use super::*;
use crate::agent_access::human::{self as graph, ReadRuntime, TaskBrief};
use crate::store::User;
use serde::Deserialize;
use std::collections::HashMap;

use aq_dashboard_contract::review_view::{
    Enrichment, ReviewMetadata, changes, choices, evidence_refs, next_text, order_responses,
    response_outcome,
};
const PREVIOUS: i64 = 16;
const BRIEF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Per-page graph brief cache: one bounded read per distinct task.
pub(super) struct Briefs {
    state: AppState,
    pub(super) reader: Uuid,
    pub(super) at: u64,
    actor: Option<User>,
    runtime: Option<ReadRuntime>,
    cache: HashMap<Uuid, Option<TaskBrief>>,
    down: bool,
    pub(super) missed: bool,
}
impl Briefs {
    pub(super) async fn new(
        state: &AppState,
        reader: Uuid,
        enabled: bool,
        at: u64,
    ) -> Result<Self> {
        let actor = if enabled && state.agent_access.is_some() {
            state
                .store
                .find_user(reader)
                .await
                .map_err(|_| AppError::Internal)?
        } else {
            None
        };
        Ok(Self {
            state: state.clone(),
            reader,
            at,
            runtime: actor
                .as_ref()
                .map(|_| ReadRuntime::with_pool(state.store.pool.clone())),
            actor,
            cache: HashMap::new(),
            down: false,
            missed: false,
        })
    }
    pub(super) fn lookups(&self) -> usize {
        self.cache.len()
    }
    pub(super) async fn get(&mut self, project: Uuid, task: Uuid) -> Option<TaskBrief> {
        let (Some(actor), Some(runtime)) = (&self.actor, &self.runtime) else {
            self.missed = true;
            return None;
        };
        if let Some(hit) = self.cache.get(&task) {
            self.missed |= hit.is_none();
            return hit.clone();
        }
        if self.down {
            self.missed = true;
            return None;
        }
        let brief = match tokio::time::timeout(
            BRIEF_TIMEOUT,
            graph::task_brief(&self.state, runtime, actor, project, task),
        )
        .await
        {
            Ok(Ok(brief)) => Some(brief),
            Ok(Err(_)) => None,
            Err(_) => {
                self.down = true;
                None
            }
        };
        self.missed |= brief.is_none();
        self.cache.insert(task, brief.clone());
        brief
    }
}

fn u64_col(v: Option<i64>) -> Result<Option<u64>> {
    v.map(|v| u64::try_from(v).map_err(|_| AppError::Internal))
        .transpose()
}
impl Service {
    pub(super) async fn meta(
        &self,
        actor: Uuid,
        decision: Uuid,
        at: u64,
    ) -> Result<ReviewMetadata> {
        let row = sqlx::query("SELECT d.requester_id,u.email,t.submitted_at_ms,d.state,f.deferred_until_ms FROM human_decision_requests d JOIN users u ON u.id=d.requester_id LEFT JOIN human_decision_submissions t ON t.decision_id=d.decision_id LEFT JOIN human_decision_deferrals f ON f.decision_id=d.decision_id AND f.actor_id=$2 WHERE d.decision_id=$1")
            .bind(decision).bind(actor).fetch_optional(&self.state.store.pool).await.map_err(db)?.ok_or_else(forbidden)?;
        let pending = row.get::<String, _>("state") == "pending";
        Ok(ReviewMetadata {
            requested_by: RequestedBy {
                user_id: row.get("requester_id"),
                email: row.get("email"),
            },
            submitted_at_ms: u64_col(row.get("submitted_at_ms"))?,
            deferred_until_ms: u64_col(row.get("deferred_until_ms"))?
                .filter(|until| pending && *until > at),
        })
    }
    /// Enrich an already authorized `inspect` view.
    pub(super) async fn enrich(
        &self,
        view: &serde_json::Value,
        briefs: &mut Briefs,
    ) -> Result<Enrichment> {
        let subject: ReviewSubject =
            serde_json::from_value(view["subject"].clone()).map_err(|_| AppError::Internal)?;
        let id: Uuid = serde_json::from_value(view["request"]["decision_id"].clone())
            .map_err(|_| AppError::Internal)?;
        let meta = self.meta(briefs.reader, id, briefs.at).await?;
        let brief = briefs.get(subject.project_id, subject.task.task_id).await;
        let title = brief.as_ref().and_then(|b| b.title.clone());
        let impact = brief.map(|b| Impact {
            blocked_tasks: b.blocked_tasks,
            truncated: b.truncated,
        });
        meta.enrich(&subject, view, (title, impact))
            .map_err(|_| AppError::Internal)
    }
    /// The full decision read: `inspect` plus the X14–X19 fields.
    pub async fn decision_view(&self, actor: &User, id: Uuid) -> Result<serde_json::Value> {
        let mut view = self.inspect(actor.id, id).await?;
        let at = now(&self.state)?;
        let mut briefs = Briefs::new(&self.state, actor.id, true, at).await?;
        let e = self.enrich(&view, &mut briefs).await?;
        let subject: ReviewSubject =
            serde_json::from_value(view["subject"].clone()).map_err(|_| AppError::Internal)?;
        let state = view["state"].as_str().ok_or(AppError::Internal)?.to_owned();
        let expires = view["request"]["expires_at_ms"]
            .as_u64()
            .ok_or(AppError::Internal)?;
        let previous = self.previous(actor.id, id, &subject).await?;
        let changes = previous.first().map(|(entry, prior)| {
            json!({"previous_decision_id":entry["decision_id"],"changes":changes(prior,&subject)})
        });
        let responses: Vec<_> = previous.into_iter().map(|(entry, _)| entry).collect();
        let evidence = self.evidence(actor.id, id, &subject).await?;
        let o = view.as_object_mut().ok_or(AppError::Internal)?;
        e.append_fields(o);
        o.insert("previous_responses".into(), json!(responses));
        o.insert("changes_since_previous".into(), json!(changes));
        o.insert(
            "what_happens_next".into(),
            json!(next_text(&state, expires, e.deferred_until_ms)),
        );
        o.insert("choices".into(), choices(state == "pending"));
        o.insert("evidence".into(), json!(evidence));
        Ok(view)
    }
    /// Up to 16 finished decisions on the same task lineage, newest first,
    /// each re-read through `inspect` so only authorized decisions appear.
    async fn previous(
        &self,
        actor: Uuid,
        id: Uuid,
        subject: &ReviewSubject,
    ) -> Result<Vec<(serde_json::Value, ReviewSubject)>> {
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT d.decision_id FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) LEFT JOIN human_decision_submissions t ON t.decision_id=d.decision_id WHERE s.subject_profile='code_review_v1' AND s.project_id=$1 AND s.workspace_id=$2 AND s.task_id=$3 AND d.state<>'pending' AND d.decision_id<>$4 ORDER BY t.submitted_at_ms DESC NULLS LAST, d.decision_id LIMIT $5")
            .bind(subject.project_id).bind(subject.task.workspace_id).bind(subject.task.task_id).bind(id).bind(PREVIOUS)
            .fetch_all(&self.state.store.pool).await.map_err(db)?;
        let mut out = Vec::new();
        for prior in ids {
            let view = match self.inspect(actor, prior).await {
                Ok(view) => view,
                Err(AppError::Forbidden(_)) => continue,
                Err(error) => return Err(error),
            };
            let receipt: DecisionReceipt =
                serde_json::from_value(view["receipt"].clone()).map_err(|_| AppError::Internal)?;
            let prior_subject: ReviewSubject =
                serde_json::from_value(view["subject"].clone()).map_err(|_| AppError::Internal)?;
            let (outcome, responder, has_comment) = response_outcome(&receipt.finality);
            let responded_by = match responder {
                Some(user) => self
                    .state
                    .store
                    .find_user(user)
                    .await
                    .map_err(|_| AppError::Internal)?
                    .map(|u| RequestedBy {
                        user_id: u.id,
                        email: u.email,
                    }),
                None => None,
            };
            out.push((
                json!({"decision_id":prior,"state":view["state"],"outcome":outcome,"responded_by":responded_by,"recorded_at_ms":receipt.recorded_at_ms,"has_comment":has_comment,"report_id":prior_subject.result.report_id,"task_version":prior_subject.task.version}),
                prior_subject,
            ));
        }
        order_responses(&mut out);
        Ok(out)
    }
    async fn evidence(
        &self,
        actor: Uuid,
        id: Uuid,
        subject: &ReviewSubject,
    ) -> Result<Vec<serde_json::Value>> {
        let viewed: HashMap<String, (String, i64)> = sqlx::query("SELECT evidence_id,ref_digest,viewed_at_ms FROM human_decision_evidence_views WHERE decision_id=$1 AND actor_id=$2")
            .bind(id).bind(actor).fetch_all(&self.state.store.pool).await.map_err(db)?
            .into_iter().map(|r| (r.get("evidence_id"), (r.get("ref_digest"), r.get("viewed_at_ms")))).collect();
        evidence_refs(subject)
            .into_iter()
            .map(|(role, artifact, link)| {
                let at = viewed
                    .get(role)
                    .filter(|(digest, _)| digest == artifact.digest.as_str())
                    .map(|(_, at)| u64::try_from(*at).map_err(|_| AppError::Internal))
                    .transpose()?;
                Ok(json!({"evidence_id":role,"ref":artifact,"link":link,"viewed_at_ms":at}))
            })
            .collect()
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeferRequest {
    operation_id: Uuid,
    expected_decision_revision: u64,
    /// `None` clears this owner's deferral.
    until_ms: Option<u64>,
}
impl DeferRequest {
    fn validate_window(&self, view: &serde_json::Value, state: &AppState) -> Result<u64> {
        let pending = view["state"] == "pending" && view["decision_revision"] == 1;
        if !pending || self.expected_decision_revision != 1 {
            return Err(conflict());
        }
        let expires = view["request"]["expires_at_ms"]
            .as_u64()
            .ok_or(AppError::Internal)?;
        let at = now(state)?;
        if let Some(until) = self.until_ms {
            let within_window = at < until && until <= expires;
            if !within_window || i64::try_from(until).is_err() {
                return Err(invalid());
            }
        }
        Ok(at)
    }
}
/// X18: a non-final, per-owner snooze. It never answers the decision.
pub(super) async fn defer_http(
    crate::authz::RequireOwner(actor): crate::authz::RequireOwner,
    axum::Extension(service): axum::Extension<Arc<Service>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    crate::admin_api::ApiJson(body): crate::admin_api::ApiJson<DeferRequest>,
) -> Result<axum::Json<serde_json::Value>> {
    if body.operation_id.is_nil() {
        return Err(invalid());
    }
    let view = service.inspect(actor.id, id).await?;
    let at = body.validate_window(&view, &service.state)?;
    let mut tx = service.tx().await?;
    Service::owner(&mut tx, actor.id).await?;
    let existing = sqlx::query("SELECT operation_id,deferred_until_ms FROM human_decision_deferrals WHERE decision_id=$1 AND actor_id=$2 FOR UPDATE")
        .bind(id).bind(actor.id).fetch_optional(&mut *tx).await.map_err(db)?;
    let replay = existing
        .as_ref()
        .filter(|r| r.get::<Uuid, _>("operation_id") == body.operation_id);
    if let Some(r) = replay {
        if u64_col(Some(r.get("deferred_until_ms")))? != body.until_ms {
            return Err(conflict());
        }
    } else {
        match body.until_ms {
            Some(until) => {
                sqlx::query("INSERT INTO human_decision_deferrals(decision_id,actor_id,operation_id,deferred_until_ms,deferred_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(decision_id,actor_id) DO UPDATE SET operation_id=EXCLUDED.operation_id,deferred_until_ms=EXCLUDED.deferred_until_ms,deferred_at_ms=EXCLUDED.deferred_at_ms")
                    .bind(id).bind(actor.id).bind(body.operation_id).bind(until as i64).bind(at as i64)
                    .execute(&mut *tx).await.map_err(db)?;
            }
            None => {
                sqlx::query(
                    "DELETE FROM human_decision_deferrals WHERE decision_id=$1 AND actor_id=$2",
                )
                .bind(id)
                .bind(actor.id)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            }
        }
    }
    tx.commit().await.map_err(db)?;
    Ok(axum::Json(
        json!({"decision_id":id,"deferred_until_ms":body.until_ms}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ViewedRequest {
    ref_digest: String,
}
/// X19: record that this owner opened one pinned evidence reference. The
/// digest must match the pin, so a view of stale evidence is not recorded.
pub(super) async fn viewed_http(
    crate::authz::RequireOwner(actor): crate::authz::RequireOwner,
    axum::Extension(service): axum::Extension<Arc<Service>>,
    axum::extract::Path((id, evidence)): axum::extract::Path<(Uuid, String)>,
    crate::admin_api::ApiJson(body): crate::admin_api::ApiJson<ViewedRequest>,
) -> Result<axum::Json<serde_json::Value>> {
    let view = service.inspect(actor.id, id).await?;
    let subject: ReviewSubject =
        serde_json::from_value(view["subject"].clone()).map_err(|_| AppError::Internal)?;
    let refs = evidence_refs(&subject);
    let (role, artifact, _) = refs
        .iter()
        .find(|(role, _, _)| *role == evidence)
        .ok_or_else(|| AppError::NotFound("evidence".into()))?;
    if artifact.digest.as_str() != body.ref_digest {
        return Err(conflict());
    }
    let at = now(&service.state)?;
    let mut tx = service.tx().await?;
    Service::owner(&mut tx, actor.id).await?;
    sqlx::query("INSERT INTO human_decision_evidence_views(decision_id,actor_id,evidence_id,ref_digest,viewed_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(decision_id,actor_id,evidence_id) DO UPDATE SET ref_digest=EXCLUDED.ref_digest,viewed_at_ms=EXCLUDED.viewed_at_ms WHERE human_decision_evidence_views.ref_digest<>EXCLUDED.ref_digest")
        .bind(id).bind(actor.id).bind(*role).bind(&body.ref_digest).bind(at as i64)
        .execute(&mut *tx).await.map_err(db)?;
    let first: i64 = sqlx::query_scalar("SELECT viewed_at_ms FROM human_decision_evidence_views WHERE decision_id=$1 AND actor_id=$2 AND evidence_id=$3")
        .bind(id).bind(actor.id).bind(*role).fetch_one(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(axum::Json(json!({"evidence_id":role,"viewed_at_ms":first})))
}
