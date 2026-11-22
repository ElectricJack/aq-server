//! Spec 0017 §5.4: the facts the offer path reads to place an attempt in an
//! environment. A failed read is no fact: the caller then chooses afresh, or
//! does not claim that no node has the environment.
use sqlx::PgPool;
use uuid::Uuid;

/// The environment `attempt` ran in, from its selection decision. A retry or
/// continuation of it keeps that environment (its workspace and build cache).
pub async fn ran_in(pool: &PgPool, attempt: Uuid) -> Option<String> {
    sqlx::query_scalar(
        "SELECT d.decision#>>'{project,environment}' FROM profiled_launches l \
         JOIN intelligence_decisions d USING(decision_id) WHERE l.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten()
}
/// Whether some node with an unexpired inventory lists `environment`
/// (`name` or `name@version`) for one of its harnesses, or has reported the
/// template of an admitted version of it ready. The environment's definition
/// pins its template digest, so a node listing it can run that template.
pub async fn listed(pool: &PgPool, environment: &str, now_ms: i64) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM intelligence_node_snapshots s, \
         jsonb_array_elements(s.snapshot->'harnesses') h \
         WHERE (s.snapshot->>'expires_at_ms')::bigint > $2 \
         AND $1 IN (h#>>'{environment,id}', (h#>>'{environment,id}')||'@'||(h#>>'{environment,version}'))) \
         OR EXISTS(SELECT 1 FROM environment_ready r \
         JOIN intelligence_node_snapshots s USING(node_id) \
         JOIN environment_versions v ON r.digests ? v.template_digest \
         WHERE (s.snapshot->>'expires_at_ms')::bigint > $2 \
         AND $1 IN (v.environment_id, v.environment_id||'@'||v.version))",
    )
    .bind(environment)
    .bind(now_ms)
    .fetch_one(pool)
    .await
    .unwrap_or(true)
}

/// The environment limits (spec 0017 §6.7): a Settings value of their own,
/// read at every use and sent to the builder inside each job. The defaults
/// are the recipe bounds and build budget of the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentLimits {
    pub max_install_steps: u64,
    pub max_step_args: u64,
    pub max_arg_bytes: u64,
    pub max_prefetch: u64,
    pub max_recipe_bytes: u64,
    pub build_wall_ms: u64,
    pub build_cpu_millis: u64,
    pub build_memory_bytes: u64,
    /// The bytes of a build or `try` log kept after redaction.
    pub log_bytes: u64,
    /// Concurrent `try` jobs per project; one more is refused.
    pub try_concurrency: u64,
    /// Admitted versions kept per environment and per base.
    pub keep_versions: u64,
}
impl Default for EnvironmentLimits {
    fn default() -> Self {
        Self {
            max_install_steps: 64,
            max_step_args: 32,
            max_arg_bytes: 4096,
            max_prefetch: 16,
            max_recipe_bytes: 1_048_576,
            build_wall_ms: 3_600_000,
            build_cpu_millis: 4000,
            build_memory_bytes: 8_589_934_592,
            log_bytes: 65_536,
            try_concurrency: 2,
            keep_versions: 3,
        }
    }
}
impl EnvironmentLimits {
    /// Every value at least 1 and within BIGINT; the message names the first that is not.
    pub fn validate(&self) -> Result<(), String> {
        let value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        for (name, n) in value.as_object().into_iter().flatten() {
            let n = n.as_u64().unwrap_or_default();
            if !(1..=i64::MAX as u64).contains(&n) {
                return Err(format!("{name} is {n}; it must be at least 1"));
            }
        }
        Ok(())
    }
}
