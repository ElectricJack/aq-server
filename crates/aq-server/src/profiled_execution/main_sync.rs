//! Spec 0016 §4, feature `delivery_playbook`: follow each bound project's
//! default branch. aq-git's `repo.retain_ref` syncs the named ref from the
//! remote and retains its head under a handle; this module records each
//! returned `{head, handle, synced_at_ms}` and offers the latest one. It
//! decides nothing about results: it only keeps the observation current.
use crate::{AppState, error::AppError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

/// `[delivery_limits]` in the vault config, until the Settings › Delivery
/// store exists. Every field is required: no limit has a built-in value.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// How often the background loop syncs every followed ref.
    pub main_sync_interval_ms: u64,
    /// An offer syncs inline when the latest sync is at least this old.
    pub main_freshness_ms: u64,
    /// Deadline of one `repo.retain_ref` call (it fetches from the remote).
    pub main_sync_deadline_ms: u64,
}
impl Limits {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.main_sync_interval_ms == 0
            || self.main_freshness_ms == 0
            || self.main_sync_deadline_ms == 0
        {
            return Err("every delivery limit must be at least 1 ms");
        }
        // A source admission is valid for at most 30 s after its observation
        // (aq-admission's bound), so an older sync could never be offered.
        if self.main_freshness_ms > 30_000 {
            return Err("main_freshness_ms cannot exceed the 30000 ms source admission window");
        }
        Ok(())
    }
}

/// One `repo.retain_ref` result for a bound project's named ref. This is the
/// `main_sync` evidence of a source admission observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncedHead {
    pub schema_version: u32,
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub reference: String,
    /// The retained commit (40 lowercase hex digits, SHA-1).
    pub head: String,
    pub retention_handle: Uuid,
    pub synced_at_ms: u64,
}
impl SyncedHead {
    pub fn validate(&self) -> bool {
        self.schema_version == 1
            && ![self.project_id, self.repository_id, self.retention_handle]
                .iter()
                .any(Uuid::is_nil)
            && (1..=i64::MAX as u64).contains(&self.binding_generation)
            && (1..=i64::MAX as u64).contains(&self.synced_at_ms)
            && self.reference.starts_with("refs/")
            && self.reference.len() <= 4096
            && super::source::hex40(&self.head)
    }
}

#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "main sync refusal");
    AppError::Conflict("followed ref sync unavailable or inconsistent".into())
}
fn now_ms(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())
}

/// Scope of one followed ref: a bound project's repository generation and ref.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub reference: &'a str,
}

/// Calls `repo.retain_ref` once under a fresh operation id and records the
/// returned head and handle. aq-git reuses the handle when the head is
/// unchanged, so repeated syncs of an unmoved ref add no row.
pub async fn sync(
    state: &AppState,
    scope: Scope<'_>,
    limits: &Limits,
) -> Result<SyncedHead, AppError> {
    let generation = i64::try_from(scope.binding_generation).map_err(|_| refused())?;
    let operation = Uuid::new_v4();
    let result = super::source::exchange(
        state,
        scope.repository_id,
        scope.binding_generation,
        "repo.retain_ref",
        json!({"ref": scope.reference}),
        Some(operation),
        limits.main_sync_deadline_ms,
    )
    .await?;
    let received = now_ms(state)?;
    let head = SyncedHead {
        schema_version: 1,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
        binding_generation: scope.binding_generation,
        reference: scope.reference.to_owned(),
        head: result["head"]["hex"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        retention_handle: result["retention_handle"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| refused())?,
        synced_at_ms: result["synced_at_ms"].as_u64().ok_or_else(|| refused())?,
    };
    if result["ref"] != scope.reference
        || result["head"]["algorithm"] != "sha1"
        || !head.validate()
        || head.synced_at_ms > received
    {
        return Err(refused());
    }
    // A handle retains exactly one head: a repeated handle with another head
    // is inconsistent and records nothing.
    let recorded = sqlx::query(
        "INSERT INTO main_sync_heads(project_id,repository_id,binding_generation,reference,retention_handle,head_hex,synced_at_ms,operation_id) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$8) \
         ON CONFLICT(project_id,repository_id,binding_generation,reference,retention_handle) DO UPDATE \
         SET synced_at_ms=GREATEST(main_sync_heads.synced_at_ms,EXCLUDED.synced_at_ms),operation_id=EXCLUDED.operation_id \
         WHERE main_sync_heads.head_hex=EXCLUDED.head_hex",
    )
    .bind(scope.project_id)
    .bind(scope.repository_id)
    .bind(generation)
    .bind(scope.reference)
    .bind(head.retention_handle)
    .bind(&head.head)
    .bind(i64::try_from(head.synced_at_ms).map_err(|_| refused())?)
    .bind(operation)
    .execute(&state.store.pool)
    .await
    .map_err(crate::error::db)?;
    if recorded.rows_affected() != 1 {
        return Err(refused());
    }
    Ok(head)
}

/// The latest recorded sync of a followed ref, if any.
pub async fn latest(state: &AppState, scope: Scope<'_>) -> Result<Option<SyncedHead>, AppError> {
    let generation = i64::try_from(scope.binding_generation).map_err(|_| refused())?;
    let row: Option<(Uuid, String, i64)> = sqlx::query_as(
        "SELECT retention_handle,head_hex,synced_at_ms FROM main_sync_heads \
         WHERE project_id=$1 AND repository_id=$2 AND binding_generation=$3 AND reference=$4 \
         ORDER BY synced_at_ms DESC, retention_handle LIMIT 1",
    )
    .bind(scope.project_id)
    .bind(scope.repository_id)
    .bind(generation)
    .bind(scope.reference)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(crate::error::db)?;
    let Some((retention_handle, head, synced_at_ms)) = row else {
        return Ok(None);
    };
    let head = SyncedHead {
        schema_version: 1,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
        binding_generation: scope.binding_generation,
        reference: scope.reference.to_owned(),
        head,
        retention_handle,
        synced_at_ms: u64::try_from(synced_at_ms).map_err(|_| refused())?,
    };
    if !head.validate() {
        return Err(refused());
    }
    Ok(Some(head))
}

/// The sync an offer uses: the latest record, or a new inline sync when there
/// is none or it is at least `main_freshness_ms` old.
pub async fn current(
    state: &AppState,
    scope: Scope<'_>,
    limits: &Limits,
) -> Result<SyncedHead, AppError> {
    let fresh = |head: &SyncedHead, now: u64| {
        now >= head.synced_at_ms && now - head.synced_at_ms < limits.main_freshness_ms
    };
    if let Some(head) = latest(state, scope).await?
        && fresh(&head, now_ms(state)?)
    {
        return Ok(head);
    }
    let head = sync(state, scope, limits).await?;
    if !fresh(&head, now_ms(state)?) {
        return Err(refused());
    }
    Ok(head)
}

/// Whether `repo.retain_ref` returned `handle` for exactly `head` on this ref.
/// Source export (policy validation) accepts such a handle for a named ref.
pub async fn handle_for<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    scope: Scope<'_>,
    handle: Uuid,
    head: &str,
) -> Result<bool, AppError> {
    let generation = i64::try_from(scope.binding_generation).map_err(|_| refused())?;
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM main_sync_heads WHERE project_id=$1 AND repository_id=$2 \
         AND binding_generation=$3 AND reference=$4 AND retention_handle=$5 AND head_hex=$6)",
    )
    .bind(scope.project_id)
    .bind(scope.repository_id)
    .bind(generation)
    .bind(scope.reference)
    .bind(handle)
    .bind(head)
    .fetch_one(executor)
    .await
    .map_err(crate::error::db)
}

/// One pass over every enabled bound project whose source is a named ref.
/// A failed project is logged and does not stop the others.
pub async fn tick(state: &AppState, limits: &Limits) -> usize {
    let Some(config) = state.config.profiled_execution.as_deref() else {
        return 0;
    };
    let mut synced = 0;
    for project in config.projects.iter().filter(|p| p.enabled) {
        let aq_admission::SourceSelection::NamedRef { reference } = &project.source else {
            continue;
        };
        let binding: Option<serde_json::Value> = match sqlx::query_scalar(
            "SELECT binding FROM project_execution_bindings WHERE project_id=$1",
        )
        .bind(project.project_id)
        .fetch_optional(&state.store.pool)
        .await
        {
            Ok(binding) => binding,
            Err(error) => {
                tracing::warn!(?error, project = %project.project_id, "main sync: binding unreadable");
                continue;
            }
        };
        let Some(binding) = binding.and_then(|b| {
            serde_json::from_value::<crate::project_bindings::domain::ProjectBinding>(b).ok()
        }) else {
            continue;
        };
        if !binding.enabled
            || binding.repository_id.is_nil()
            || binding.repository_binding_generation == 0
        {
            continue;
        }
        let scope = Scope {
            project_id: project.project_id,
            repository_id: binding.repository_id,
            binding_generation: binding.repository_binding_generation,
            reference: reference.as_str(),
        };
        match sync(state, scope, limits).await {
            Ok(_) => synced += 1,
            Err(error) => {
                tracing::warn!(?error, project = %project.project_id, "main sync deferred")
            }
        }
    }
    synced
}

/// The background loop: one pass every `main_sync_interval_ms`. Absent
/// `[delivery_limits]` or profiled execution, nothing is followed.
pub async fn run(state: AppState, mut stop: watch::Receiver<bool>) {
    let Some(limits) = state.config.delivery_limits.clone() else {
        return;
    };
    if state.config.profiled_execution.is_none() || state.source_retention.is_none() {
        return;
    }
    let mut ticks = tokio::time::interval(Duration::from_millis(limits.main_sync_interval_ms));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|value| *value) => return,
            _ = ticks.tick() => {}
        }
        tokio::select! {
            biased;
            _ = stop.wait_for(|value| *value) => return,
            synced = tick(&state, &limits) => tracing::debug!(synced, "main sync pass"),
        }
    }
}
