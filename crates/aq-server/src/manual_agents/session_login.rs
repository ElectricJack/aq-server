//! A hand-started session's subscription login, leased from and written back to
//! the server's versioned login store (`profiled_execution::login_refresh`),
//! which stays the one source of truth: no login file is configured on the node.
//!
//! `POST /v1/worker/manual-agents/{id}/login` returns the newest stored login of
//! the session's provider (octet-stream, `aq-material-version`) to the node that
//! owns the session while the owner wants it running.
//! `POST /v1/worker/manual-agents/{id}/login/refresh?base_version=N` stores the
//! body as version N+1 while N is current (origin `refresh`) and otherwise
//! answers `409` with the current version, exactly like an attempt's write-back.
use crate::{
    AppState,
    error::{AppError, db},
    profiled_execution::{
        login_refresh::{
            self, HeadSource, Lineage, Origin, api::RefreshOutcome, append_locked, codex, head,
            head_digest, head_version, identity, lineage_locked, store::MAX_BYTES,
        },
        material::source_or_configured,
        material_api::VERSION_HEADER,
    },
    worker_api::{authenticate_context, lock_current_node},
};
use aq_intelligence_config::LoadedRegistry;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use sqlx::{Postgres, Transaction};
use std::sync::Arc;
use uuid::Uuid;
use zeroize::Zeroizing;

fn refused() -> AppError {
    AppError::Conflict("session login is not currently authorized".into())
}

pub(crate) fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/worker/manual-agents/{id}/login", post(lease))
        .route("/v1/worker/manual-agents/{id}/login/refresh", post(refresh))
        .layer(DefaultBodyLimit::max(MAX_BYTES))
        .with_state(state)
}

/// The session's login lineage, locked for the rest of the transaction.
struct Held {
    tx: Transaction<'static, Postgres>,
    registry: Arc<LoadedRegistry>,
    index: usize,
    lineage: Lineage,
    provider: String,
}

/// The calling node owns session `id`, it is not stopped, and `live` accepts its
/// (status, desired_running); the login is the node's permitted subscription of
/// the provider the session's own spec names.
async fn held(
    state: &AppState,
    headers: &HeaderMap,
    id: Uuid,
    live: fn(&str, bool) -> bool,
) -> Result<Held, AppError> {
    let auth = authenticate_context(state, headers).await?;
    if auth.delegated().is_some() {
        return Err(AppError::Forbidden("An enrolled worker is required".into()));
    }
    let registry = state.config.intelligence.clone().ok_or_else(refused)?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    lock_current_node(&mut tx, &auth).await?;
    let session: Option<(Option<String>, String, bool)> = sqlx::query_as(
        "SELECT spec->>'provider',status,desired_running FROM manual_agent_sessions WHERE id=$1 AND node_id=$2 AND stopped_at_ms IS NULL FOR SHARE",
    )
    .bind(id)
    .bind(auth.id())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    let Some((Some(provider), status, desired)) = session else {
        return Err(refused());
    };
    if !live(&status, desired) {
        return Err(refused());
    }
    crate::intelligence::fence_current_registry(state, &mut tx).await?;
    let (index, credential) = registry
        .subscription_for(auth.id(), &provider)
        .ok_or_else(|| {
            AppError::Conflict(
                "No permitted server subscription is available for this provider".into(),
            )
        })?;
    let base = registry.fingerprints[index].clone().ok_or_else(refused)?;
    let lineage = lineage_locked(
        &mut tx,
        &identity(&credential.profile),
        credential.revision,
        &base,
    )
    .await?;
    Ok(Held {
        tx,
        registry,
        index,
        lineage,
        provider,
    })
}

async fn lease(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, AppError> {
    let mut held = held(&state, &headers, id, |status, desired| {
        desired && status != "pending"
    })
    .await?;
    let source = match (head(&held.lineage), &held.registry.config.material_store) {
        (Some(entry), Some(dir)) => Some(HeadSource::of(entry, dir)),
        (Some(_), None) => return Err(refused()),
        (None, _) => None,
    };
    let (registry, index) = (held.registry.clone(), held.index);
    let bytes = tokio::task::spawn_blocking(move || {
        source_or_configured(&registry, index, source.as_ref())
    })
    .await
    .map_err(|_| AppError::Internal)?
    .map_err(|_| AppError::Conflict("Server subscription login needs attention".into()))?;
    let now = (state.clock)().timestamp_millis();
    let refusal = login_refresh::lease_refusal(&mut held.tx, &held.provider, bytes.bytes(), now);
    if let Some(reason) = refusal.await? {
        return Err(AppError::Conflict(reason));
    }
    held.tx.commit().await.map_err(db)?;
    // As for an attempt's material: this one response copy leaves the server.
    let mut response = bytes.bytes().to_vec().into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        VERSION_HEADER,
        HeaderValue::from(head_version(&held.lineage)),
    );
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Based {
    base_version: i64,
}

async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    (Path(id), Query(Based { base_version })): (Path<Uuid>, Query<Based>),
    body: Bytes,
) -> Result<Response, AppError> {
    let bytes = Arc::new(Zeroizing::new(body.to_vec()));
    drop(body);
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err(refused());
    }
    let mut held = held(&state, &headers, id, |status, _| {
        matches!(status, "running" | "stopping")
    })
    .await?;
    // Only a Codex login rotates; a Claude token is never written back.
    let dir = held.registry.config.material_store.clone();
    let Some(dir) = dir.filter(|_| held.provider == codex::PROVIDER) else {
        return Err(refused());
    };
    let current = head_version(&held.lineage);
    let outcome = |accepted, version, replayed| RefreshOutcome {
        accepted,
        version,
        replayed,
    };
    let outcome = if head_digest(&held.lineage) == aq_admission::Digest::of(&bytes).as_str() {
        // Already the newest version (a repeated write-back).
        outcome(true, current, true)
    } else if base_version != current {
        // Someone newer won: keep the current version.
        outcome(false, current, false)
    } else if !login_refresh::refresh_of_head(
        held.registry.clone(),
        held.index,
        held.lineage.head_source(&dir),
        bytes.clone(),
    )
    .await?
    {
        return Err(refused());
    } else {
        let now = (state.clock)().timestamp_millis();
        let (version, _) = append_locked(
            &mut held.tx,
            &dir,
            &held.lineage,
            aq_login_store::NewRevision {
                origin: Origin::Refresh,
                lease_id: None,
                bytes,
                now_ms: now,
            },
        )
        .await?;
        held.tx.commit().await.map_err(db)?;
        outcome(true, version, false)
    };
    let status = if outcome.accepted {
        StatusCode::OK
    } else {
        StatusCode::CONFLICT
    };
    Ok((status, Json(outcome)).into_response())
}
