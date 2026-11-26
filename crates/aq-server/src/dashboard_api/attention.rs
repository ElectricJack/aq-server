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

/// Cleared entries kept per user; the oldest are forgotten beyond this.
const KEPT: i64 = 5000;

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
        "/api/v1/attention/dismissals",
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
    body: std::result::Result<Json<view::DismissAttention>, JsonRejection>,
) -> Result<Json<view::AttentionDismissal>> {
    let Json(body) = body.map_err(|_| Error::Invalid)?;
    request.reply(2, || Ok(clear(&request, body))).await
}

/// Idempotent: a repeated request succeeds with `newly_dismissed: false`. An id the
/// overview no longer lists (stale or already resolved) can still be cleared.
async fn clear(
    request: &DashboardRequest,
    body: view::DismissAttention,
) -> Result<view::AttentionDismissal> {
    let (state, runtime, user) = request.parts();
    let a = auth(state, runtime, user).await?;
    if !a.owner {
        return Err(crate::error::AppError::Forbidden("owner required".into()).into());
    }
    if !view::dismissible(&body.attention_id) {
        return Err(Error::NotDismissible);
    }
    let now = service::stamp(state)? as i64;
    let pool = &state.store.pool;
    let inserted = sqlx::query("INSERT INTO dashboard_attention_dismissals(principal_id,attention_id,dismissed_at_ms) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(user.id).bind(&body.attention_id).bind(now).execute(pool).await?.rows_affected();
    sqlx::query("DELETE FROM dashboard_attention_dismissals WHERE principal_id=$1 AND attention_id IN (SELECT attention_id FROM dashboard_attention_dismissals WHERE principal_id=$1 ORDER BY dismissed_at_ms DESC,attention_id OFFSET $2)")
        .bind(user.id).bind(KEPT).execute(pool).await?;
    service::fence(state, runtime, user, &a).await?;
    Ok(view::AttentionDismissal {
        attention_id: body.attention_id,
        dismissed: true,
        newly_dismissed: inserted > 0,
    })
}
