//! Why the last offer of a task was refused before any attempt existed
//! (spec 0014 §6.1): the startup text is over its byte limit, or the
//! environment has no platform text. The offer path records it; the work
//! read lists it as a readiness reason, the way a missing level is listed.
//! One row per task, removed when a later offer of the task composes; the
//! table is a rolling set of the newest `refusals_kept` tasks.
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfferRefusal {
    /// The assembled startup text is `bytes` long; `limit` is the smaller of
    /// the execution profile's `startup_context_bytes` (`profile_limit`) and
    /// the project's `context_bytes` ceiling (`project_ceiling`).
    StartupOverLimit {
        bytes: u64,
        limit: u64,
        execution_profile: String,
        profile_limit: u64,
        project_ceiling: u64,
    },
    /// The server ships no platform text for `environment`.
    PlatformTextMissing { environment: String },
    #[serde(rename = "delivery_ancestry_wait")]
    DeliveryAncestry { wait: DeliveryAncestryWait },
    /// Spec 0017 §5.4: `environment` lacks the `missing` labels of the
    /// task's `aq:needs`.
    #[cfg(feature = "project_environments")]
    EnvironmentMissingCapability {
        environment: String,
        missing: Vec<String>,
    },
    /// Spec 0017 §5.4: no node's inventory lists `environment`.
    #[cfg(feature = "project_environments")]
    NoNodeHasEnvironment { environment: String },
    /// The offer needs more than one slot of node `node_id` provides.
    SlotResourcesShort {
        node_id: Uuid,
        required: SlotResources,
        available: SlotResources,
    },
}

/// What an offer needs from one slot, or what one slot of a node provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotResources {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
}
impl SlotResources {
    pub fn new(cpu_millis: u64, memory_bytes: u64, disk_bytes: u64) -> Self {
        Self {
            cpu_millis,
            memory_bytes,
            disk_bytes,
        }
    }
    /// The refusal when this need exceeds what a slot of `node_id` provides.
    pub fn short_on(self, available: Self, node_id: Uuid) -> Option<OfferRefusal> {
        let short = self.cpu_millis > available.cpu_millis
            || self.memory_bytes > available.memory_bytes
            || self.disk_bytes > available.disk_bytes;
        short.then_some(OfferRefusal::SlotResourcesShort {
            node_id,
            required: self,
            available,
        })
    }
    fn shortfall(self, available: Self) -> String {
        let gib = |bytes: u64| format!("{:.1} GiB ({bytes} bytes)", bytes as f64 / 1073741824.0);
        let mut parts = vec![];
        if self.memory_bytes > available.memory_bytes {
            let (need, have) = (gib(self.memory_bytes), gib(available.memory_bytes));
            parts.push(format!("memory {need} needed, {have} available"));
        }
        if self.cpu_millis > available.cpu_millis {
            let (need, have) = (self.cpu_millis, available.cpu_millis);
            parts.push(format!("cpu {need} millicores needed, {have} available"));
        }
        if self.disk_bytes > available.disk_bytes {
            let (need, have) = (gib(self.disk_bytes), gib(available.disk_bytes));
            parts.push(format!("disk {need} needed, {have} available"));
        }
        parts.join("; ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryAncestryWait {
    GraphContextIncomplete,
    PrerequisiteIncomplete {
        prerequisite: Uuid,
    },
    /// The completed prerequisite pushed a result that no recorded merge
    /// delivered to the followed branch.
    PrerequisiteNotDelivered {
        prerequisite: Uuid,
    },
    MergeEvidenceUnavailable,
    MainSyncUnavailable,
    AncestryUnavailable {
        prerequisite: Uuid,
        merge_sha: String,
        base: String,
    },
    MergeMissingFromBase {
        prerequisite: Uuid,
        merge_sha: String,
        base: String,
    },
    SourceBaseChanged,
}

impl DeliveryAncestryWait {
    fn message(&self) -> String {
        match self {
            Self::GraphContextIncomplete => {
                "the Graph context does not list all prerequisites".into()
            }
            Self::PrerequisiteIncomplete { prerequisite } => {
                format!("Graph prerequisite {prerequisite} is still open")
            }
            Self::PrerequisiteNotDelivered { prerequisite } => format!(
                "Graph prerequisite {prerequisite} is complete but its result is not merged yet"
            ),
            Self::MergeEvidenceUnavailable => "delivery merge evidence could not be read".into(),
            Self::MainSyncUnavailable => {
                "the repository's followed branch could not be refreshed and retained".into()
            }
            Self::AncestryUnavailable {
                prerequisite,
                merge_sha,
                base,
            } => format!(
                "ancestry could not be verified for prerequisite {prerequisite} (merge {merge_sha}, retained base {base})"
            ),
            Self::MergeMissingFromBase {
                prerequisite,
                merge_sha,
                base,
            } => format!(
                "retained base {base} does not contain prerequisite {prerequisite}'s merge {merge_sha}"
            ),
            Self::SourceBaseChanged => {
                "the retained export differs from the currently verified source base".into()
            }
        }
    }
}

impl OfferRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::StartupOverLimit { .. } => "startup_over_limit",
            Self::PlatformTextMissing { .. } => "platform_text_missing",
            Self::DeliveryAncestry { .. } => "delivery_ancestry_wait",
            #[cfg(feature = "project_environments")]
            Self::EnvironmentMissingCapability { .. } => "environment_missing_capability",
            #[cfg(feature = "project_environments")]
            Self::NoNodeHasEnvironment { .. } => "no_node_has_environment",
            Self::SlotResourcesShort { .. } => "slot_resources_short",
        }
    }
    /// Which of the two limits is the one exceeded.
    fn limiting(&self) -> &'static str {
        match self {
            Self::StartupOverLimit {
                profile_limit,
                project_ceiling,
                ..
            } if project_ceiling < profile_limit => "project_context_bytes",
            _ => "startup_context_bytes",
        }
    }
    pub fn message(&self) -> String {
        match self {
            Self::StartupOverLimit {
                bytes,
                limit,
                execution_profile,
                ..
            } => {
                let which = match self.limiting() {
                    "project_context_bytes" => "the project's context_bytes ceiling".to_string(),
                    _ => {
                        format!("startup_context_bytes of execution profile `{execution_profile}`")
                    }
                };
                format!(
                    "AQ does not offer this task: its startup text is {bytes} bytes, over the limit of {limit} bytes ({which}). Shorten the task, the role or the project's notes, or raise that limit."
                )
            }
            Self::PlatformTextMissing { environment } => format!(
                "AQ does not offer this task: the server has no platform text for environment `{environment}`, and an agent is not launched without it."
            ),
            Self::DeliveryAncestry { wait } => format!(
                "AQ is waiting to offer this task because {}.",
                wait.message()
            ),
            #[cfg(feature = "project_environments")]
            Self::EnvironmentMissingCapability {
                environment,
                missing,
            } => format!(
                "AQ does not offer this task: it needs {}, which environment `{environment}` does not provide, and no other environment of the project provides everything it needs. Change the task's needs or the project's environments.",
                missing.join(", ")
            ),
            #[cfg(feature = "project_environments")]
            Self::NoNodeHasEnvironment { environment } => format!(
                "AQ does not offer this task yet: no node lists environment `{environment}` in its inventory. It is offered once a node can run that environment."
            ),
            Self::SlotResourcesShort {
                node_id,
                required,
                available,
            } => format!(
                "AQ does not offer this task on node {node_id}: one of its slots is too small ({}). Lower the task's resource needs or give the node's slots more.",
                required.shortfall(*available)
            ),
        }
    }
    /// The reason as the work view lists it.
    pub fn shown(&self, recorded_at_ms: i64) -> Value {
        let mut shown = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        shown["message"] = json!(self.message());
        shown["recorded_at_ms"] = json!(recorded_at_ms);
        if matches!(self, Self::StartupOverLimit { .. }) {
            shown["limiting"] = json!(self.limiting());
        }
        shown
    }
}

/// What a failed startup assembly means for the offer. `None` for a failure
/// that is not about the documents (an empty task or role text), which the
/// offer path refuses without a reason of this kind.
pub fn of_startup(
    error: &aq_profiles::roles::StartupError,
    execution: &aq_profiles::roles::Execution,
    project_ceiling: u64,
    environment: &str,
) -> Option<OfferRefusal> {
    use aq_profiles::roles::{StartupError, StartupPart};
    match error {
        StartupError::OverLimit { bytes, limit } => Some(OfferRefusal::StartupOverLimit {
            bytes: *bytes,
            limit: *limit,
            execution_profile: execution.id.clone(),
            profile_limit: execution.limits.startup_context_bytes,
            project_ceiling,
        }),
        StartupError::Empty(StartupPart::Platform) => Some(OfferRefusal::PlatformTextMissing {
            environment: environment.into(),
        }),
        StartupError::Empty(_) => None,
    }
}
/// The prefix of [`OfferRefusal::refusal`].
const REFUSED: &str = "offer_refused";
/// Whether `reason` is an [`OfferRefusal::refusal`]: the offer path skips
/// the task and goes on with the next.
pub fn is_refusal(reason: &str) -> bool {
    reason.starts_with(REFUSED)
}
impl OfferRefusal {
    /// The refusal text of the offer path.
    pub fn refusal(&self) -> String {
        format!("{REFUSED} {}: {}", self.code(), self.message())
    }
}

#[derive(Clone, Copy)]
pub struct RefusedTask {
    pub task: Uuid,
    pub project: Uuid,
}

/// Record the refusal of `task`'s offer, replacing an earlier one, and keep
/// the newest `refusals_kept` tasks. A failure only logs: the offer is
/// refused either way.
pub async fn record(pool: &PgPool, task: RefusedTask, refusal: &OfferRefusal, now_ms: i64) {
    if let Err(error) = store(pool, task, refusal, now_ms).await {
        tracing::warn!(task=%task.task, %error, "the offer refusal was not recorded");
    }
}
async fn store(
    pool: &PgPool,
    subject: RefusedTask,
    refusal: &OfferRefusal,
    now_ms: i64,
) -> Result<()> {
    let RefusedTask { task, project } = subject;
    let kept = crate::settings(pool).await?.limits.refusals_kept;
    let value = serde_json::to_value(refusal).map_err(|_| crate::Error::Internal)?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO task_offer_refusals(task_id,project_id,refusal,recorded_at_ms) VALUES($1,$2,$3,$4)
         ON CONFLICT(task_id) DO UPDATE SET project_id=EXCLUDED.project_id,refusal=EXCLUDED.refusal,recorded_at_ms=EXCLUDED.recorded_at_ms
         WHERE task_offer_refusals.refusal IS DISTINCT FROM EXCLUDED.refusal",
    )
    .bind(task)
    .bind(project)
    .bind(value)
    .bind(now_ms)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM task_offer_refusals WHERE task_id NOT IN
         (SELECT task_id FROM task_offer_refusals ORDER BY recorded_at_ms DESC,task_id LIMIT $1)",
    )
    .bind(i64::from(kept))
    .execute(&mut *tx)
    .await?;
    Ok(tx.commit().await?)
}
/// A later offer of `task` composed: nothing to show. A failure only logs;
/// the next composed offer clears it.
pub async fn clear(pool: &PgPool, task: Uuid) {
    clear_matching(pool, task, true).await;
}

/// Composition resolves startup refusals. Keep an ancestry wait until the
/// current Graph prerequisites and the exact export pass the publication fence,
/// and a slot that is too small until an offer is published.
pub async fn clear_after_compose(pool: &PgPool, task: Uuid) {
    clear_matching(pool, task, false).await;
}

async fn clear_matching(pool: &PgPool, task: Uuid, include_ancestry: bool) {
    let cleared = sqlx::query("DELETE FROM task_offer_refusals WHERE task_id=$1 AND ($2 OR refusal->>'code' NOT IN ('delivery_ancestry_wait','slot_resources_short'))")
        .bind(task)
        .bind(include_ancestry)
        .execute(pool)
        .await;
    if let Err(error) = cleared {
        tracing::warn!(%task, %error, "the offer refusal was not cleared");
    }
}
/// The recorded refusal of `task`, as the work view lists it. A failed read
/// and a row this version cannot read both show nothing.
pub async fn shown(pool: &PgPool, task: Uuid) -> Option<Value> {
    let row: Option<(Value, i64)> =
        sqlx::query_as("SELECT refusal,recorded_at_ms FROM task_offer_refusals WHERE task_id=$1")
            .bind(task)
            .fetch_optional(pool)
            .await
            .ok()?;
    let (refusal, at) = row?;
    let refusal: OfferRefusal = serde_json::from_value(refusal).ok()?;
    Some(refusal.shown(at))
}

#[cfg(test)]
mod tests {
    use super::OfferRefusal;

    #[test]
    fn the_reason_names_both_numbers_and_the_limit_that_was_exceeded() {
        let over = |profile_limit: u64, project_ceiling: u64| OfferRefusal::StartupOverLimit {
            bytes: 9000,
            limit: profile_limit.min(project_ceiling),
            execution_profile: "sandbox".into(),
            profile_limit,
            project_ceiling,
        };
        let shown = over(4096, 1 << 20).shown(7);
        assert_eq!(shown["code"], "startup_over_limit");
        assert_eq!(
            (shown["bytes"].as_u64(), shown["limit"].as_u64()),
            (Some(9000), Some(4096))
        );
        assert_eq!(shown["limiting"], "startup_context_bytes");
        assert_eq!(shown["execution_profile"], "sandbox");
        assert_eq!(shown["recorded_at_ms"], 7);
        let message = shown["message"].as_str().unwrap();
        assert!(
            message.contains("9000 bytes") && message.contains("4096 bytes"),
            "{message}"
        );
        assert!(message.contains("startup_context_bytes of execution profile `sandbox`"));
        let shown = over(8192, 2048).shown(7);
        assert_eq!(shown["limiting"], "project_context_bytes");
        assert!(
            shown["message"]
                .as_str()
                .unwrap()
                .contains("context_bytes ceiling")
        );
        let missing = OfferRefusal::PlatformTextMissing {
            environment: "lab".into(),
        };
        assert_eq!(missing.shown(1)["code"], "platform_text_missing");
        assert!(missing.message().contains("environment `lab`"));
    }

    #[test]
    fn a_slot_too_small_for_the_memory_names_memory_and_both_values() {
        use super::SlotResources;
        let slot = |memory_bytes| SlotResources::new(2000, memory_bytes, 1 << 30);
        let node = uuid::Uuid::new_v4();
        let (needed, offered) = (slot(6 << 30), slot(4 << 30));
        assert_eq!(offered.short_on(offered, node), None);
        let refusal = needed.short_on(offered, node).unwrap();
        let shown = refusal.shown(7);
        assert_eq!(shown["code"], "slot_resources_short");
        assert_eq!(shown["node_id"], node.to_string());
        assert_eq!(shown["required"]["memory_bytes"], 6u64 << 30);
        assert_eq!(shown["available"]["memory_bytes"], 4u64 << 30);
        let message = refusal.message();
        assert!(
            message.contains(&format!("node {node}"))
                && message.contains(
                    "memory 6.0 GiB (6442450944 bytes) needed, 4.0 GiB (4294967296 bytes) available"
                ),
            "{message}"
        );
        assert!(
            !message.contains("cpu") && !message.contains("disk"),
            "{message}"
        );
    }
}
