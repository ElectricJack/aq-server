//! Following main (spec 0016 §4, feature `delivery_playbook`). Central aq-git
//! syncs a project's default branch from its remote and retains the head
//! (`repo.retain_ref`). This module makes that call, records each retained
//! head in `profiled_main_syncs`, and answers two questions: which head an
//! offer starts from, and whether a handle is one a sync returned for a head.
//! It decides nothing about results. Every limit is a Settings value
//! (`delivery-limits`).
use crate::{
    DeliveryError, OperationContext, Result,
    protocol::{self, WireResult},
    source_checks::hex40,
};
use aq_role_settings::delivery::DeliveryLimits;
use aq_worker_protocol::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgExecutor, PgPool, Row};
use std::{path::Path, time::Duration};
use tokio::sync::watch;
use uuid::Uuid;

mod ancestry;
pub use ancestry::{OfferAncestry, dependent_current};

/// The named ref of one repository binding that a project follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub reference: String,
}
impl Scope {
    pub fn new(
        project_id: Uuid,
        repository_id: Uuid,
        binding_generation: u64,
        reference: &str,
    ) -> Self {
        let reference = reference.to_owned();
        Self {
            project_id,
            repository_id,
            binding_generation,
            reference,
        }
    }
    fn validate(&self) -> Result<()> {
        if self.project_id.is_nil()
            || self.repository_id.is_nil()
            || !(1..=i64::MAX as u64).contains(&self.binding_generation)
            || !self.reference.starts_with("refs/heads/")
            || self.reference.len() > 1024
            || self.reference.chars().any(char::is_control)
        {
            return Err(DeliveryError::InvalidInput("invalid main sync scope"));
        }
        Ok(())
    }
}

/// One head central aq-git synced from the remote and retained for a scope.
/// It is also the evidence an admission carries for a followed named ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainSync {
    pub schema_version: u32,
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub reference: String,
    /// The retained commit (40 lowercase hex digits).
    pub head: String,
    /// The aq-git retention handle `repo.retain_ref` returned for `head`.
    pub retention_handle: Uuid,
    /// When aq-git synced the ref (its reply), in milliseconds.
    pub synced_at_ms: u64,
}
impl MainSync {
    pub fn scope(&self) -> Scope {
        Scope {
            project_id: self.project_id,
            repository_id: self.repository_id,
            binding_generation: self.binding_generation,
            reference: self.reference.clone(),
        }
    }
    pub fn validate(&self) -> bool {
        self.schema_version == 1
            && self.scope().validate().is_ok()
            && hex40(&self.head)
            && !self.retention_handle.is_nil()
            && (1..=i64::MAX as u64).contains(&self.synced_at_ms)
    }
    /// Synced less than `freshness_ms` before `now_ms`, and not after it.
    pub fn fresh(&self, now_ms: u64, freshness_ms: u64) -> bool {
        now_ms >= self.synced_at_ms && now_ms - self.synced_at_ms < freshness_ms
    }
    /// This record proves that `scope` resolved to `head`, retained by
    /// `handle`, no later than `observed_at_ms`.
    pub fn proves(&self, scope: &Scope, head: &str, handle: Uuid, observed_at_ms: u64) -> bool {
        self.validate()
            && self.scope() == *scope
            && self.head == head
            && self.retention_handle == handle
            && self.synced_at_ms <= observed_at_ms
    }
}

/// Admission evidence (a serialized [`MainSync`]) proves `scope` resolved to
/// `head`, retained by `handle`, no later than `observed_at_ms`.
pub fn proves(
    evidence: Option<&Value>,
    scope: &Scope,
    head: &str,
    handle: Uuid,
    observed_at_ms: u64,
) -> bool {
    evidence
        .and_then(|value| serde_json::from_value::<MainSync>(value.clone()).ok())
        .is_some_and(|synced| synced.proves(scope, head, handle, observed_at_ms))
}

/// The `repo.retain_ref` result: `{ref, head:{algorithm,hex}, retention_handle, synced_at_ms}`.
#[derive(Debug, Clone, Deserialize)]
struct Retained {
    #[serde(rename = "ref")]
    reference: String,
    head: ObjectId,
    retention_handle: Uuid,
    synced_at_ms: u64,
}
impl WireResult for Retained {
    fn validate(&self, _: &OperationContext, _: &str, params: &Value) -> bool {
        params["ref"] == self.reference.as_str()
            && hex40(&self.head.hex)
            && !self.retention_handle.is_nil()
            && (1..=i64::MAX as u64).contains(&self.synced_at_ms)
    }
}

/// The delivery limits from Settings (their defaults until saved).
pub async fn limits(pool: &PgPool) -> Result<DeliveryLimits> {
    aq_role_settings::delivery_limits(pool)
        .await
        .map(|(limits, _)| limits)
        .map_err(|_| DeliveryError::InvalidInput("delivery limits unavailable"))
}

/// Sync `scope`'s ref from the remote and retain its head under one new
/// operation, waiting at most `wait_ms`, then record the head.
pub async fn retain(
    pool: &PgPool,
    socket: &Path,
    scope: &Scope,
    wait_ms: u64,
    now_ms: impl Fn() -> u64,
) -> Result<MainSync> {
    scope.validate()?;
    let context = OperationContext {
        operation_id: Uuid::new_v4(),
        repository_id: scope.repository_id,
        binding_generation: scope.binding_generation,
    };
    context.validate()?;
    let params = json!({"ref": scope.reference});
    let mut request = serde_json::to_vec(&json!({"protocol_version":1,
        "request_id":context.operation_id,"operation":"repo.retain_ref",
        "repository_id":scope.repository_id,"operation_id":context.operation_id,
        "expected_binding_generation":scope.binding_generation,"params":params,
        "deadline_ms":wait_ms}))?;
    request.push(b'\n');
    let raw = protocol::exchange_within(socket, &request, Duration::from_millis(wait_ms)).await?;
    let receipt = protocol::decode::<Retained>(raw, &context, "repo.retain_ref", &params)?;
    let retained = receipt.outcome.map_err(|_| DeliveryError::Transport)?;
    let synced = MainSync {
        schema_version: 1,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
        binding_generation: scope.binding_generation,
        reference: retained.reference,
        head: retained.head.hex,
        retention_handle: retained.retention_handle,
        synced_at_ms: retained.synced_at_ms,
    };
    let now = now_ms();
    if !synced.validate() || synced.synced_at_ms > now {
        return Err(DeliveryError::Protocol);
    }
    record(pool, &synced, now).await?;
    Ok(synced)
}

/// Record one retained head. A handle keeps the head it was first recorded
/// with; aq-git reuses a handle only for an unchanged head.
pub async fn record(pool: &PgPool, synced: &MainSync, now_ms: u64) -> Result<()> {
    if !synced.validate() || now_ms > i64::MAX as u64 {
        return Err(DeliveryError::InvalidInput("invalid main sync"));
    }
    let kept: Option<String> = sqlx::query_scalar(
        "INSERT INTO profiled_main_syncs(project_id,repository_id,binding_generation,reference,\
         retention_handle,head,synced_at_ms,recorded_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8) \
         ON CONFLICT(project_id,repository_id,binding_generation,reference,retention_handle) \
         DO UPDATE SET synced_at_ms=GREATEST(profiled_main_syncs.synced_at_ms,EXCLUDED.synced_at_ms),\
         recorded_at_ms=EXCLUDED.recorded_at_ms WHERE profiled_main_syncs.head=EXCLUDED.head \
         RETURNING head",
    )
    .bind(synced.project_id)
    .bind(synced.repository_id)
    .bind(synced.binding_generation as i64)
    .bind(&synced.reference)
    .bind(synced.retention_handle)
    .bind(&synced.head)
    .bind(synced.synced_at_ms as i64)
    .bind(now_ms as i64)
    .fetch_optional(pool)
    .await?;
    if kept.as_deref() != Some(synced.head.as_str()) {
        return Err(DeliveryError::IdempotencyConflict);
    }
    Ok(())
}

/// The most recently synced head recorded for `scope`.
pub async fn latest<'e>(executor: impl PgExecutor<'e>, scope: &Scope) -> Result<Option<MainSync>> {
    scope.validate()?;
    let row = sqlx::query(
        "SELECT head,retention_handle,synced_at_ms FROM profiled_main_syncs \
         WHERE project_id=$1 AND repository_id=$2 AND binding_generation=$3 AND reference=$4 \
         ORDER BY synced_at_ms DESC,recorded_at_ms DESC LIMIT 1",
    )
    .bind(scope.project_id)
    .bind(scope.repository_id)
    .bind(scope.binding_generation as i64)
    .bind(&scope.reference)
    .fetch_optional(executor)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let synced = MainSync {
        schema_version: 1,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
        binding_generation: scope.binding_generation,
        reference: scope.reference.clone(),
        head: row.get("head"),
        retention_handle: row.get("retention_handle"),
        synced_at_ms: u64::try_from(row.get::<i64, _>("synced_at_ms"))
            .map_err(|_| DeliveryError::Protocol)?,
    };
    if !synced.validate() {
        return Err(DeliveryError::Protocol);
    }
    Ok(Some(synced))
}

/// `handle` is one a sync of `scope` returned for exactly `head`.
pub async fn accepts<'e>(
    executor: impl PgExecutor<'e>,
    scope: &Scope,
    handle: Uuid,
    head: &str,
) -> Result<bool> {
    scope.validate()?;
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_main_syncs WHERE project_id=$1 AND repository_id=$2 \
         AND binding_generation=$3 AND reference=$4 AND retention_handle=$5 AND head=$6)",
    )
    .bind(scope.project_id)
    .bind(scope.repository_id)
    .bind(scope.binding_generation as i64)
    .bind(&scope.reference)
    .bind(handle)
    .bind(head)
    .fetch_one(executor)
    .await?)
}

/// The head an offer for `scope` starts from: the latest record while it is
/// younger than `main_freshness_ms`, otherwise one synced now.
pub async fn current(
    pool: &PgPool,
    socket: &Path,
    scope: &Scope,
    now_ms: impl Fn() -> u64,
) -> Result<MainSync> {
    let limits = limits(pool).await?;
    if let Some(synced) = cached(pool, scope, &limits, &now_ms).await? {
        return Ok(synced);
    }
    refresh(pool, socket, scope, &limits, now_ms).await
}

async fn cached(
    pool: &PgPool,
    scope: &Scope,
    limits: &DeliveryLimits,
    now_ms: impl Fn() -> u64,
) -> Result<Option<MainSync>> {
    Ok(latest(pool, scope)
        .await?
        .filter(|synced| synced.fresh(now_ms(), limits.main_freshness_ms)))
}

async fn refresh(
    pool: &PgPool,
    socket: &Path,
    scope: &Scope,
    limits: &DeliveryLimits,
    now_ms: impl Fn() -> u64,
) -> Result<MainSync> {
    let synced = retain(pool, socket, scope, limits.git_transfer_timeout_ms, &now_ms).await?;
    if !synced.fresh(now_ms(), limits.main_freshness_ms) {
        return Err(DeliveryError::Protocol);
    }
    Ok(synced)
}

/// The scopes of `projects` (project, followed named ref) whose execution
/// binding exists and is enabled.
pub async fn bound(pool: &PgPool, projects: &[(Uuid, String)]) -> Result<Vec<Scope>> {
    let ids: Vec<Uuid> = projects.iter().map(|(id, _)| *id).collect();
    let rows = sqlx::query(
        "SELECT project_id,binding FROM project_execution_bindings WHERE project_id=ANY($1)",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let mut scopes = Vec::new();
    for row in rows {
        let (project, binding): (Uuid, Value) = (row.get("project_id"), row.get("binding"));
        let repository = binding["repository_id"]
            .as_str()
            .and_then(|s| s.parse().ok());
        let generation = binding["repository_binding_generation"].as_u64();
        let (Some(repository_id), Some(binding_generation)) = (repository, generation) else {
            continue;
        };
        if binding["enabled"] != true {
            continue;
        }
        for (_, reference) in projects.iter().filter(|(id, _)| *id == project) {
            scopes.push(Scope {
                project_id: project,
                repository_id,
                binding_generation,
                reference: reference.clone(),
            });
        }
    }
    Ok(scopes)
}

/// One pass of the sync loop: sync every bound scope; returns how many
/// synced. One project's failure does not stop the others.
pub async fn sync_bound(
    pool: &PgPool,
    socket: &Path,
    projects: &[(Uuid, String)],
    now_ms: impl Fn() -> u64,
) -> Result<usize> {
    let limits = limits(pool).await?;
    let mut synced = 0;
    for scope in bound(pool, projects).await? {
        match retain(
            pool,
            socket,
            &scope,
            limits.git_transfer_timeout_ms,
            &now_ms,
        )
        .await
        {
            Ok(_) => synced += 1,
            Err(error) => {
                tracing::warn!(project = %scope.project_id, %error, "main sync deferred")
            }
        }
    }
    Ok(synced)
}

/// The background loop: a pass now, then one every `main_sync_interval_ms`
/// (read from Settings before each wait), until `stop`. `clock` is now.
pub async fn run(
    pool: PgPool,
    socket: std::path::PathBuf,
    projects: Vec<(Uuid, String)>,
    clock: impl Fn() -> chrono::DateTime<chrono::Utc>,
    mut stop: watch::Receiver<bool>,
) {
    if projects.is_empty() {
        return;
    }
    let now_ms = || u64::try_from(clock().timestamp_millis()).unwrap_or(0);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stop| *stop) => return,
            result = sync_bound(&pool, &socket, &projects, &now_ms) => {
                if let Err(error) = result { tracing::warn!(%error, "main sync pass deferred"); }
            }
        }
        let interval = limits(&pool)
            .await
            .unwrap_or_default()
            .main_sync_interval_ms;
        tokio::select! {
            biased;
            _ = stop.wait_for(|stop| *stop) => return,
            _ = tokio::time::sleep(Duration::from_millis(interval)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn synced() -> MainSync {
        MainSync {
            schema_version: 1,
            project_id: Uuid::new_v4(),
            repository_id: Uuid::new_v4(),
            binding_generation: 2,
            reference: "refs/heads/main".into(),
            head: "a".repeat(40),
            retention_handle: Uuid::new_v4(),
            synced_at_ms: 1000,
        }
    }
    #[test]
    fn a_record_is_fresh_only_inside_the_settings_window() {
        let s = synced();
        assert!(s.fresh(1000, 30_000));
        assert!(s.fresh(30_999, 30_000));
        assert!(!s.fresh(31_000, 30_000));
        assert!(!s.fresh(999, 30_000), "synced after now is not fresh");
        assert!(s.fresh(1500, 1000) && !s.fresh(2000, 1000));
    }
    #[test]
    fn a_record_proves_only_its_exact_scope_head_and_handle() {
        let s = synced();
        let (scope, head, handle) = (s.scope(), s.head.clone(), s.retention_handle);
        assert!(s.proves(&scope, &head, handle, 1000));
        assert!(
            !s.proves(&scope, &head, handle, 999),
            "observed before the sync"
        );
        assert!(!s.proves(&scope, &"b".repeat(40), handle, 2000));
        assert!(!s.proves(&scope, &head, Uuid::new_v4(), 2000));
        let mut other = scope.clone();
        other.reference = "refs/heads/other".into();
        assert!(!s.proves(&other, &head, handle, 2000));
        other = scope.clone();
        other.binding_generation = 3;
        assert!(!s.proves(&other, &head, handle, 2000));
        for bad in [
            MainSync {
                schema_version: 2,
                ..s.clone()
            },
            MainSync {
                head: "0".repeat(40),
                ..s.clone()
            },
            MainSync {
                retention_handle: Uuid::nil(),
                ..s.clone()
            },
            MainSync {
                synced_at_ms: 0,
                ..s.clone()
            },
            MainSync {
                reference: "refs/tags/v1".into(),
                ..s.clone()
            },
        ] {
            assert!(!bad.validate(), "{bad:?}");
        }
    }
    #[test]
    fn the_retain_ref_reply_must_name_the_requested_ref_and_a_real_handle() {
        let context = OperationContext {
            operation_id: Uuid::new_v4(),
            repository_id: Uuid::new_v4(),
            binding_generation: 1,
        };
        let params = json!({"ref":"refs/heads/main"});
        let reply = |result: Value| {
            let mut raw = serde_json::to_vec(&json!({"protocol_version":1,
                "request_id":context.operation_id,"operation_id":context.operation_id,
                "ok":true,"result":result}))
            .unwrap();
            raw.push(b'\n');
            protocol::decode::<Retained>(raw, &context, "repo.retain_ref", &params)
        };
        let valid = json!({"ref":"refs/heads/main","head":{"algorithm":"sha1","hex":"a".repeat(40)},
            "retention_handle":Uuid::new_v4(),"synced_at_ms":1000});
        assert!(reply(valid.clone()).unwrap().outcome.is_ok());
        for (field, bad) in [
            ("ref", json!("refs/heads/other")),
            ("head", json!({"algorithm":"sha1","hex":"0".repeat(40)})),
            ("head", json!({"algorithm":"sha256","hex":"a".repeat(64)})),
            ("retention_handle", json!(Uuid::nil())),
            ("synced_at_ms", json!(0)),
        ] {
            let mut value = valid.clone();
            value[field] = bad;
            assert!(reply(value).is_err(), "{field}");
        }
    }
}
