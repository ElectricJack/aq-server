//! First-offer ancestry for the exact revision retained from the followed ref.
use super::{MainSync, Scope, cached, limits, refresh};
use crate::{
    DeliveryError, OperationContext, Result,
    commands::GitTarget,
    protocol::{self, WireResult},
    source_checks::hex40,
};
use aq_role_settings::refusals::DeliveryAncestryWait as Wait;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};
use tokio::time::Instant;
use uuid::Uuid;

/// Select a retained head that includes every same-repository delivery from
/// the task's completed direct Graph blockers. An inconclusive cached read
/// can mean the merge object has yet to be fetched, so refresh in that case too.
pub async fn dependent_current(
    pool: &PgPool,
    socket: &Path,
    scope: &Scope,
    task: Uuid,
    context: &Value,
    now_ms: impl Fn() -> u64,
) -> std::result::Result<MainSync, Wait> {
    let required = OfferAncestry::load(pool, scope.into(), task, context).await?;
    let limits = limits(pool).await.map_err(sync_failed)?;
    if let Some(head) = cached(pool, scope, &limits, &now_ms)
        .await
        .map_err(sync_failed)?
        && required.check(socket, &head.head).await.is_ok()
    {
        return Ok(head);
    }
    required
        .retain_and_check(pool, socket, scope, &limits, now_ms)
        .await
}

impl From<&Scope> for GitTarget {
    fn from(scope: &Scope) -> Self {
        Self {
            repository_id: scope.repository_id,
            binding_generation: scope.binding_generation,
        }
    }
}

/// Durable same-repository merges required by the task's current Graph context.
pub struct OfferAncestry {
    repository: GitTarget,
    merges: Vec<(Uuid, String)>,
}

impl OfferAncestry {
    pub async fn load(
        pool: &PgPool,
        repository: GitTarget,
        task: Uuid,
        context: &Value,
    ) -> std::result::Result<Self, Wait> {
        let blockers = completed_blockers(task, context)?;
        let merges = if blockers.is_empty() {
            Vec::new()
        } else {
            sqlx::query_as(
                "SELECT DISTINCT task_id, merge_sha FROM delivery_merge_evidence \
                 WHERE task_id=ANY($1) AND target #>> '{git,repository_id}'=$2 \
                 ORDER BY task_id, merge_sha",
            )
            .bind(blockers)
            .bind(repository.repository_id.to_string())
            .fetch_all(pool)
            .await
            .map_err(|error| {
                tracing::warn!(%task, %error, "reading prerequisite merge evidence failed");
                Wait::MergeEvidenceUnavailable
            })?
        };
        Ok(Self { repository, merges })
    }

    /// Preserve the selected base when its ancestry is sufficient. A moving
    /// source can refresh after a failed proof. An immutable source can wait.
    pub async fn verify_or_refresh(
        &self,
        pool: &PgPool,
        socket: &Path,
        base: &str,
        moving: Option<&Scope>,
        now_ms: impl Fn() -> u64,
    ) -> std::result::Result<Option<MainSync>, Wait> {
        match self.check(socket, base).await {
            Ok(()) => Ok(None),
            Err(wait) => {
                let Some(scope) = moving else {
                    return Err(wait);
                };
                let limits = limits(pool).await.map_err(sync_failed)?;
                self.retain_and_check(pool, socket, scope, &limits, now_ms)
                    .await
                    .map(Some)
            }
        }
    }

    async fn retain_and_check(
        &self,
        pool: &PgPool,
        socket: &Path,
        scope: &Scope,
        limits: &super::DeliveryLimits,
        now_ms: impl Fn() -> u64,
    ) -> std::result::Result<MainSync, Wait> {
        if GitTarget::from(scope) != self.repository {
            return Err(Wait::MainSyncUnavailable);
        }
        let head = refresh(pool, socket, scope, limits, now_ms)
            .await
            .map_err(sync_failed)?;
        self.check(socket, &head.head).await?;
        Ok(head)
    }

    async fn check(&self, socket: &Path, base: &str) -> std::result::Result<(), Wait> {
        check(socket, &self.repository, base, &self.merges).await
    }
}

fn sync_failed(error: DeliveryError) -> Wait {
    tracing::warn!(%error, "retaining main for an offer failed");
    Wait::MainSyncUnavailable
}

fn completed_blockers(task: Uuid, context: &Value) -> std::result::Result<Vec<Uuid>, Wait> {
    if context["truncated"] != false {
        return Err(Wait::GraphContextIncomplete);
    }
    let nodes: BTreeMap<_, _> = context["nodes"]
        .as_array()
        .ok_or(Wait::GraphContextIncomplete)?
        .iter()
        .filter_map(|node| node["id"].as_str().map(|id| (id, node)))
        .collect();
    let target = task.to_string();
    let edges = context["edges"]
        .as_array()
        .ok_or(Wait::GraphContextIncomplete)?;
    let mut blockers = BTreeSet::new();
    for edge in edges.iter().filter(|edge| {
        edge["edge_type"] == "blocks" && edge["deleted"] == false && edge["target"] == target
    }) {
        let source = edge["source"]
            .as_str()
            .ok_or(Wait::GraphContextIncomplete)?;
        let node = nodes.get(source).ok_or(Wait::GraphContextIncomplete)?;
        let prerequisite = Uuid::parse_str(source).map_err(|_| Wait::GraphContextIncomplete)?;
        if node["kind"] != "task" || node["task_state"] != "completed" || node["deleted"] != false {
            return Err(Wait::PrerequisiteIncomplete { prerequisite });
        }
        blockers.insert(prerequisite);
    }
    Ok(blockers.into_iter().collect())
}

async fn check(
    socket: &Path,
    repository: &GitTarget,
    base: &str,
    merges: &[(Uuid, String)],
) -> std::result::Result<(), Wait> {
    let deadline = Instant::now() + Duration::from_secs(5);
    for (prerequisite, sha) in merges {
        let result = contains(
            socket,
            repository,
            sha,
            base,
            deadline.saturating_duration_since(Instant::now()),
        )
        .await;
        let prerequisite = *prerequisite;
        match result {
            Ok(true) => {}
            Ok(false) => {
                return Err(Wait::MergeMissingFromBase {
                    prerequisite,
                    merge_sha: sha.clone(),
                    base: base.into(),
                });
            }
            Err(_) => {
                return Err(Wait::AncestryUnavailable {
                    prerequisite,
                    merge_sha: sha.clone(),
                    base: base.into(),
                });
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Ancestry {
    contains: bool,
    semantics: String,
}
impl WireResult for Ancestry {
    fn validate(&self, _: &OperationContext, _: &str, _: &Value) -> bool {
        self.semantics == "strict_commit_ancestry_v1"
    }
}

async fn contains(
    socket: &Path,
    repository: &GitTarget,
    ancestor: &str,
    descendant: &str,
    wait: Duration,
) -> Result<bool> {
    if !hex40(ancestor) || wait.as_millis() == 0 {
        return Err(DeliveryError::Protocol);
    }
    let context = OperationContext {
        operation_id: Uuid::new_v4(),
        repository_id: repository.repository_id,
        binding_generation: repository.binding_generation,
    };
    let params = json!({
        "ancestor": {"algorithm": "sha1", "hex": ancestor},
        "descendant": {"algorithm": "sha1", "hex": descendant},
    });
    // The v1 aq-git repository ancestry operation is spelled "contains".
    let mut request = serde_json::to_vec(&json!({
        "protocol_version": 1, "request_id": context.operation_id, "operation": "contains",
        "repository_id": repository.repository_id, "expected_binding_generation": repository.binding_generation,
        "params": params, "deadline_ms": wait.as_millis(),
    }))?;
    request.push(b'\n');
    let raw = protocol::exchange_within(socket, &request, wait).await?;
    protocol::decode::<Ancestry>(raw, &context, "contains", &params)?
        .outcome
        .map(|ancestry| ancestry.contains)
        .map_err(|_| DeliveryError::Transport)
}
