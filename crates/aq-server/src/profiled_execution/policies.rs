//! Startup-owned source-policy installation. No user identity is borrowed and no
//! worker request can install configuration. Omitted policies remain tombstones.
use super::config::{ProfiledConfig, ProjectPolicyRecord};
use crate::{AppState, error::AppError};
use aq_admission::Digest;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
const OWNER_KEY: &str = "profiled_policy_configuration_v1";
const MAX_RETAINED: usize = 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigurationOwner {
    schema_version: u32,
    revision: u64,
    configuration_digest: Digest,
}
#[derive(Debug, PartialEq, Eq)]
pub struct InstallReport {
    pub configured: bool,
    pub revision: Option<u64>,
    pub installed: u32,
    pub disabled: u32,
    pub unbound: u32,
}
fn conflict(message: &str) -> AppError {
    AppError::Conflict(message.into())
}
use crate::error::db;
fn record(
    config: &ProfiledConfig,
    configuration_digest: &Digest,
    project: &super::config::ProjectPolicy,
) -> ProjectPolicyRecord {
    ProjectPolicyRecord {
        schema_version: 1,
        project_id: project.project_id,
        revision: config.revision,
        configuration_digest: configuration_digest.clone(),
        enabled: project.enabled,
        priority: project.priority,
        source: project.source.clone(),
        retention_handle: project.retention_handle,
    }
}
/// Call before dispatch and after newly configured projects become bound. Same
/// revision/content is idempotent; a higher explicit revision retires omissions.
/// Removing the config section is a local no-op, not global policy retirement.
/// To retire all policies install a higher revision with an empty projects list.
pub async fn install(state: &AppState) -> Result<InstallReport, AppError> {
    let Some(config) = state.config.profiled_execution.as_deref() else {
        return Ok(InstallReport {
            configured: false,
            revision: None,
            installed: 0,
            disabled: 0,
            unbound: 0,
        });
    };
    config
        .validate()
        .map_err(|_| conflict("invalid profiled policy configuration"))?;
    let configuration_digest = config
        .configuration_digest()
        .map_err(|_| AppError::Internal)?;
    let configured: BTreeMap<_, _> = config
        .projects
        .iter()
        .map(|p| (p.project_id, record(config, &configuration_digest, p)))
        .collect();
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('aq-profiled-policy-install-v1',0))",
    )
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    let previous: Option<String> =
        sqlx::query_scalar("SELECT value FROM meta WHERE key=$1 FOR UPDATE")
            .bind(OWNER_KEY)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
    if let Some(previous) = previous {
        if previous.len() > 4096 {
            return Err(AppError::Internal);
        }
        let previous: ConfigurationOwner =
            serde_json::from_str(&previous).map_err(|_| AppError::Internal)?;
        if previous.schema_version != 1
            || previous.revision == 0
            || previous.revision > i64::MAX as u64
        {
            return Err(AppError::Internal);
        }
        if config.revision < previous.revision
            || (config.revision == previous.revision
                && configuration_digest != previous.configuration_digest)
        {
            return Err(conflict(
                "profiled configuration revision is stale or has changed content",
            ));
        }
    }
    let rows=sqlx::query("SELECT project_id,revision,policy_digest,policy FROM admission_source_policies ORDER BY project_id LIMIT 1025").fetch_all(&mut *tx).await.map_err(db)?;
    if rows.len() > MAX_RETAINED {
        return Err(conflict("retained project policy quota exhausted"));
    }
    let mut old = BTreeMap::new();
    for row in rows {
        let project: Uuid = row.get("project_id");
        let prior: ProjectPolicyRecord = serde_json::from_value(row.get("policy"))
            .map_err(|_| conflict("existing source policy requires explicit owner migration"))?;
        let reference = prior.artifact().map_err(|_| AppError::Internal)?;
        if prior.project_id != project
            || prior.revision as i64 != row.get::<i64, _>("revision")
            || reference.digest.as_str() != row.get::<String, _>("policy_digest")
        {
            return Err(AppError::Internal);
        }
        if prior.revision > config.revision {
            return Err(conflict(
                "installed project policy is newer than configuration",
            ));
        }
        old.insert(project, prior);
    }
    let requested: BTreeSet<_> = old.keys().chain(configured.keys()).copied().collect();
    // Project predicates precede source rows in the admission lock order.
    // Missing projects are reported ineligible; no project binding is fabricated.
    let ids: Vec<_> = requested.iter().copied().collect();
    let bound:Vec<Uuid>=sqlx::query_scalar("SELECT project_id FROM project_execution_bindings WHERE project_id=ANY($1) ORDER BY project_id FOR SHARE").bind(&ids).fetch_all(&mut *tx).await.map_err(db)?;
    let bound: BTreeSet<_> = bound.into_iter().collect();
    let new_count = configured
        .keys()
        .filter(|id| bound.contains(id) && !old.contains_key(id))
        .count();
    if old.len() + new_count > MAX_RETAINED {
        return Err(conflict("retained project policy quota exhausted"));
    }
    let mut report = InstallReport {
        configured: true,
        revision: Some(config.revision),
        installed: 0,
        disabled: 0,
        unbound: 0,
    };
    for project in requested {
        let next = if let Some(next) = configured.get(&project) {
            if !bound.contains(&project) {
                report.unbound += 1;
                continue;
            }
            next.clone()
        } else {
            let prior = old.get(&project).ok_or(AppError::Internal)?;
            ProjectPolicyRecord {
                revision: config.revision,
                configuration_digest: configuration_digest.clone(),
                enabled: false,
                ..prior.clone()
            }
        };
        let reference = next.artifact().map_err(|_| AppError::Internal)?;
        let body = serde_json::to_value(&next).map_err(|_| AppError::Internal)?;
        if serde_json::to_vec(&body)
            .map_err(|_| AppError::Internal)?
            .len()
            > 65536
        {
            return Err(conflict("project policy record bound"));
        }
        if let Some(prior) = old.get(&project) {
            let old_reference = prior.artifact().map_err(|_| AppError::Internal)?;
            if prior.revision == config.revision {
                if old_reference != reference {
                    return Err(conflict("project policy changed at the same revision"));
                }
                continue;
            }
            sqlx::query("UPDATE admission_source_policies SET revision=$2,policy_digest=$3,policy=$4 WHERE project_id=$1").bind(project).bind(config.revision as i64).bind(reference.digest.as_str()).bind(body).execute(&mut *tx).await.map_err(db)?;
        } else {
            sqlx::query("INSERT INTO admission_source_policies(project_id,revision,policy_digest,policy) VALUES($1,$2,$3,$4)").bind(project).bind(config.revision as i64).bind(reference.digest.as_str()).bind(body).execute(&mut *tx).await.map_err(db)?;
        }
        if next.enabled {
            report.installed += 1;
        } else {
            report.disabled += 1;
        }
    }
    let owner = ConfigurationOwner {
        schema_version: 1,
        revision: config.revision,
        configuration_digest,
    };
    sqlx::query("INSERT INTO meta(key,value) VALUES($1,$2) ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value").bind(OWNER_KEY).bind(serde_json::to_string(&owner).map_err(|_|AppError::Internal)?).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(report)
}

/// Fence the complete installed configuration, including runtime allowlists.
/// Caller holds the node lock and must call before taking project/policy locks:
/// the installer owns configuration -> project -> policy lock order.
pub async fn fence_configuration_locked(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &ProfiledConfig,
) -> Result<(), AppError> {
    let value: String = sqlx::query_scalar("SELECT value FROM meta WHERE key=$1 FOR SHARE")
        .bind(OWNER_KEY)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(|| conflict("profiled configuration is not installed"))?;
    if value.len() > 4096 {
        return Err(AppError::Internal);
    }
    let owner: ConfigurationOwner = serde_json::from_str(&value).map_err(|_| AppError::Internal)?;
    if owner.schema_version != 1
        || owner.revision != config.revision
        || owner.configuration_digest
            != config
                .configuration_digest()
                .map_err(|_| AppError::Internal)?
    {
        return Err(conflict("profiled configuration is stale"));
    }
    Ok(())
}
