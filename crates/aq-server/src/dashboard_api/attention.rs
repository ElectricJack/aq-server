//! The complete "Needs you" list (View All) and clearing an informational entry.
//!
//! The list is the overview's own attention list, read through the same owners and
//! the same cleared-state filter, so the overview count and this list cannot
//! disagree. Clearing records a per-user notification state only: it never
//! resolves, approves or cancels the record behind the entry, and an entry that
//! needs a decision cannot be cleared at all.
use super::{
    DashboardRequest, MountedOwners, ReadQuery, aggregation, contract::attention as view,
    contract::*, service::auth, *,
};
use axum::extract::{
    DefaultBodyLimit,
    rejection::{JsonRejection, QueryRejection},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AttentionQuery {
    scope: Option<String>,
    kind: Option<String>,
    limit: Option<u16>,
    cursor: Option<String>,
}

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/api/v1/attention", get(list)).route(
        "/api/v1/attention/{attention_id}/dismiss",
        post(dismiss).layer(DefaultBodyLimit::max(4096)),
    )
}

async fn list(
    owners: Option<Extension<MountedOwners>>,
    request: DashboardRequest,
    query: std::result::Result<Query<AttentionQuery>, QueryRejection>,
) -> Result<Json<Envelope<view::AttentionPage>>> {
    let Query(query) = query.map_err(|_| Error::Invalid)?;
    request
        .reply(2, || {
            Ok(page(
                &request,
                query,
                owners.map(|Extension(v)| v).unwrap_or_default(),
            ))
        })
        .await
}

async fn page(
    request: &DashboardRequest,
    query: AttentionQuery,
    mounted: MountedOwners,
) -> Result<Envelope<view::AttentionPage>> {
    let AttentionQuery {
        scope,
        kind,
        limit,
        cursor,
    } = query;
    let read = ReadQuery {
        scope,
        limit,
        cursor: None,
    };
    let (scope, limit) = (read.scope()?, usize::from(read.limit()?));
    let kind = kind
        .map(|k| AttentionKind::parse(&k).ok_or(Error::Invalid))
        .transpose()?;
    let overview = aggregation::overview(request, scope, mounted).await?;
    let (items, next_cursor, matching) = view::page(
        overview.data.attention,
        kind.as_ref(),
        cursor.as_deref(),
        limit,
    )
    .map_err(|_| Error::Invalid)?;
    let known = overview.data.counts.needs_you;
    let total = match known.value {
        // An unavailable decision owner leaves the count unknown; never show it as zero.
        None => Total::unknown(),
        Some(_) => Total {
            value: Some(matching as u32),
            accuracy: known.accuracy,
        },
    };
    Ok(Envelope {
        api_version: overview.api_version,
        request_id: overview.request_id,
        server_time: overview.server_time,
        scope: overview.scope,
        data: view::AttentionPage {
            has_more: next_cursor.is_some(),
            items,
            next_cursor,
            total,
            sources: overview.data.sources,
        },
        observation: overview.observation,
        pending_operations: overview.pending_operations,
        coverage: overview.coverage,
    })
}

async fn dismiss(
    request: DashboardRequest,
    Path(id): Path<String>,
    body: std::result::Result<Json<view::DismissAttention>, JsonRejection>,
) -> Result<Json<view::AttentionDismissal>> {
    let Json(body) = body.map_err(|_| Error::Invalid)?;
    request.reply(2, || Ok(clear(&request, id, body))).await
}

fn check_revision(expected: &str, current: Option<&str>) -> Result<()> {
    view::check_revision(expected, current).map_err(|error| match error {
        view::RevisionError::Invalid => Error::Invalid,
        view::RevisionError::Changed => Error::RevisionChanged,
    })
}

/// Pending review requests are gates even if their presentation is a failed
/// result or internal notification. Expiry alone does not resolve a gate.
pub(super) async fn review_gate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    subject: Uuid,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE d.state='pending' AND (s.report_id=$1 OR s.task_id=$1 OR d.decision_id=$1))")
        .bind(subject).fetch_one(&mut **tx).await?)
}

/// Scope is owner-only, matching bootstrap and the overview. Never infer a
/// project grant from a caller-supplied attention id, including on replay.
async fn clear(
    request: &DashboardRequest,
    id: String,
    body: view::DismissAttention,
) -> Result<view::AttentionDismissal> {
    use sqlx::Row;
    let (state, runtime, user) = request.parts();
    let a = auth(state, runtime, user).await?;
    if !a.owner {
        return Err(crate::error::AppError::Forbidden("owner required".into()).into());
    }
    if !view::dismissible(&id) {
        return Err(Error::NotDismissible);
    }
    check_revision(&body.expected_revision, None)?;
    let mut parts = id.split(':');
    let kind = parts.next().ok_or(Error::Invalid)?;
    let subject: Uuid = parts
        .next()
        .ok_or(Error::Invalid)?
        .parse()
        .map_err(|_| Error::Invalid)?;
    if subject.is_nil() {
        return Err(Error::Invalid);
    }
    let mut tx = state.store.pool.begin().await?;
    let current = match kind {
        "internal_work" if parts.next().is_none() => {
            let row = sqlx::query("SELECT xmin::text AS revision,affected_kind,affected_id FROM internal_tasks WHERE id=$1 FOR SHARE")
                .bind(subject).fetch_optional(&mut *tx).await?;
            if let Some(row) = &row {
                let affected_kind: String = row.get("affected_kind");
                if matches!(affected_kind.as_str(), "decision" | "question") {
                    return Err(Error::NotDismissible);
                }
                if let Ok(affected) = row.get::<String, _>("affected_id").parse::<Uuid>()
                    && review_gate(&mut tx, affected).await?
                {
                    return Err(Error::NotDismissible);
                }
            }
            row.map(|r| r.get::<String, _>("revision"))
        }
        "result" => {
            let project: Uuid = parts
                .next()
                .ok_or(Error::Invalid)?
                .parse()
                .map_err(|_| Error::Invalid)?;
            if parts.next().is_some() || project.is_nil() {
                return Err(Error::Invalid);
            }
            // Read the exact retained subject, not a bounded overview sample.
            let row = sqlx::query("SELECT r.report_digest,l.execution_spec#>>'{lifecycle,project_id}' AS project FROM profiled_result_reports r JOIN profiled_launches l USING(attempt_id) WHERE r.report_id=$1 FOR SHARE OF r")
                .bind(subject).fetch_optional(&mut *tx).await?;
            if let Some(row) = &row
                && row.get::<String, _>("project") != project.to_string()
            {
                return Err(Error::Invalid);
            }
            row.map(|r| format!("sha256:{}", r.get::<String, _>("report_digest")))
        }
        "worker" | "operation" => {
            let project = parts.next().ok_or(Error::Invalid)?;
            if (!project.is_empty() && project.parse::<Uuid>().is_err()) || parts.next().is_some() {
                return Err(Error::Invalid);
            }
            if kind == "worker" {
                sqlx::query_scalar("SELECT xmin::text FROM nodes WHERE id=$1 FOR SHARE")
                    .bind(subject)
                    .fetch_optional(&mut *tx)
                    .await?
            } else {
                sqlx::query_scalar("SELECT request_digest FROM dashboard_operations WHERE operation_id=$1 AND principal_id=$2 FOR SHARE")
                    .bind(subject).bind(user.id).fetch_optional(&mut *tx).await?
            }
        }
        _ => return Err(Error::Invalid),
    };
    check_revision(&body.expected_revision, current.as_deref())?;
    if review_gate(&mut tx, subject).await? {
        return Err(Error::NotDismissible);
    }
    // Fence before mutation and hold the principal lock through commit. A
    // revoked owner cannot replay an old dismissal or persist a new one.
    let owner: bool = sqlx::query_scalar("SELECT role='owner' FROM users WHERE id=$1 FOR SHARE")
        .bind(user.id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
    if !owner {
        return Err(Error::AuthorizationChanged);
    }
    service::fence(state, runtime, user, &a).await?;
    let now = service::stamp(state)? as i64;
    let at: i64 = sqlx::query_scalar("INSERT INTO dashboard_attention_dismissals(principal_id,attention_id,expected_revision,dismissed_at_ms) VALUES($1,$2,$3,$4) ON CONFLICT(principal_id,attention_id,expected_revision) DO UPDATE SET dismissed_at_ms=dashboard_attention_dismissals.dismissed_at_ms RETURNING dismissed_at_ms")
        .bind(user.id).bind(&id).bind(&body.expected_revision).bind(now).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(view::AttentionDismissal {
        attention_id: id,
        expected_revision: body.expected_revision,
        dismissed: true,
        dismissed_at_ms: at,
    })
}

/// Apply the same revision and review protection on every overview refresh.
/// A newly opened gate must reappear even if its notification was dismissed.
pub(super) async fn filter(runtime: &Runtime, principal: Uuid, data: &mut Overview) -> Result<()> {
    use sqlx::Row;
    let ids: Vec<_> = data.attention.iter().map(|a| a.id.clone()).collect();
    let subjects: Vec<_> = data.attention.iter().map(|a| a.subject.id).collect();
    let mut tx = service::tx(runtime).await?;
    let rows = sqlx::query("SELECT attention_id,expected_revision FROM dashboard_attention_dismissals WHERE principal_id=$1 AND attention_id=ANY($2)")
        .bind(principal).bind(&ids).fetch_all(&mut *tx).await?;
    let cleared: std::collections::BTreeSet<(String, String)> = rows
        .into_iter()
        .map(|r| (r.get("attention_id"), r.get("expected_revision")))
        .collect();
    let gates = sqlx::query("SELECT s.report_id,s.task_id,d.decision_id FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE d.state='pending' AND (s.report_id=ANY($1) OR s.task_id=ANY($1) OR d.decision_id=ANY($1))")
        .bind(&subjects).fetch_all(&mut *tx).await?;
    let gates: std::collections::BTreeSet<Uuid> = gates
        .into_iter()
        .flat_map(|r| [r.get("report_id"), r.get("task_id"), r.get("decision_id")])
        .collect();
    let internal: Vec<Uuid> = ids
        .iter()
        .filter_map(|id| id.strip_prefix("internal_work:")?.parse().ok())
        .collect();
    // Some affected records open the internal run rather than the record itself.
    // Protect their gates too, exactly as clear() checks affected_id on writes.
    let protected: Vec<Uuid> = sqlx::query_scalar("SELECT t.id FROM internal_tasks t WHERE t.id=ANY($1) AND (t.affected_kind IN ('decision','question') OR EXISTS(SELECT 1 FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE d.state='pending' AND t.affected_id IN (s.report_id::text,s.task_id::text,d.decision_id::text)))")
        .bind(&internal).fetch_all(&mut *tx).await?;
    let protected: std::collections::BTreeSet<String> = protected
        .into_iter()
        .map(|id| format!("internal_work:{id}"))
        .collect();
    tx.commit().await?;
    view::filter_dismissed(data, &cleared, &gates, &protected);
    Ok(())
}
