//! Following main (spec 0016 §4, feature `delivery_playbook`). Central aq-git
//! syncs a project's default branch from its remote and retains the head
//! (`repo.retain_ref`). This module makes that call, records each retained
//! head in `profiled_main_syncs`, and answers two questions: which head an
//! offer starts from, and whether a handle is one a sync returned for a head.
//! It decides nothing about results. Every limit is a Settings value
//! (`delivery-limits`).
use crate::{DeliveryError, OperationContext, Result, protocol::WireResult, source_checks::hex40};
use aq_role_settings::delivery::DeliveryLimits;
use aq_worker_protocol::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgExecutor, PgPool, Row};
use uuid::Uuid;

mod ancestry;
mod follower;
pub use ancestry::OfferAncestry;
pub use follower::{MainFollower, sync_bound};

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
    fn valid_identity(&self) -> bool {
        !self.project_id.is_nil()
            && !self.repository_id.is_nil()
            && (1..=i64::MAX as u64).contains(&self.binding_generation)
    }
    fn valid_branch(&self) -> bool {
        self.reference.starts_with("refs/heads/")
            && self.reference.len() <= 1024
            && !self.reference.chars().any(char::is_control)
    }
    fn validate(&self) -> Result<()> {
        if !self.valid_identity() || !self.valid_branch() {
            return Err(DeliveryError::InvalidInput("invalid main sync scope"));
        }
        Ok(())
    }
}

/// The retained commit and handle named by an admission at its observation time.
#[derive(Clone, Copy)]
pub struct ObservedHead<'a> {
    pub head: &'a str,
    pub handle: Uuid,
    pub observed_at_ms: u64,
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
    /// This record independently confirms the scope, retained commit, handle and observation time.
    pub fn proves(&self, scope: &Scope, observation: ObservedHead<'_>) -> bool {
        self.validate()
            && self.scope() == *scope
            && self.head == observation.head
            && self.retention_handle == observation.handle
            && self.synced_at_ms <= observation.observed_at_ms
    }
}

/// Check the admission observation against a serialized [`MainSync`].
pub fn proves(evidence: Option<&Value>, scope: &Scope, observation: ObservedHead<'_>) -> bool {
    evidence
        .and_then(|value| serde_json::from_value::<MainSync>(value.clone()).ok())
        .is_some_and(|synced| synced.proves(scope, observation))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol;
    use serde_json::json;
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
        let scope = s.scope();
        let observed = ObservedHead {
            head: &s.head,
            handle: s.retention_handle,
            observed_at_ms: 1000,
        };
        assert!(s.proves(&scope, observed));
        assert!(
            !s.proves(
                &scope,
                ObservedHead {
                    observed_at_ms: 999,
                    ..observed
                }
            ),
            "observed before the sync"
        );
        let later = ObservedHead {
            observed_at_ms: 2000,
            ..observed
        };
        assert!(!s.proves(
            &scope,
            ObservedHead {
                head: &"b".repeat(40),
                ..later
            }
        ));
        assert!(!s.proves(
            &scope,
            ObservedHead {
                handle: Uuid::new_v4(),
                ..later
            }
        ));
        let mut other = scope.clone();
        other.reference = "refs/heads/other".into();
        assert!(!s.proves(&other, later));
        other = scope.clone();
        other.binding_generation = 3;
        assert!(!s.proves(&other, later));
    }

    #[test]
    fn malformed_sync_evidence_is_rejected() {
        let s = synced();
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
