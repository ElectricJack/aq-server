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
    /// Claims of one job whose lease ran out without a result; at the next
    /// claim the server completes it as failed (step `fetch`) instead.
    pub max_job_claims: u64,
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
            max_job_claims: 3,
        }
    }
}
/// The longest duration limit: the server adds it to `now()` as a
/// PostgreSQL interval (a claim's lease is `build_wall_ms`).
pub const MAX_DURATION_MS: u64 = 3_155_695_200_000;
/// The shortest `build_wall_ms`: the builder keeps a tenth of it as its
/// timeout margin, and redaction needs time.
pub const MIN_BUILD_WALL_MS: u64 = 60_000;
impl EnvironmentLimits {
    /// Every value at least 1 and within BIGINT, a duration (`*_ms`) within
    /// [`MAX_DURATION_MS`] and `build_wall_ms` at least [`MIN_BUILD_WALL_MS`];
    /// the message names the first that is not.
    pub fn validate(&self) -> Result<(), String> {
        let value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        for (name, n) in value.as_object().into_iter().flatten() {
            let n = n.as_u64().unwrap_or_default();
            if name == "build_wall_ms" && n < MIN_BUILD_WALL_MS {
                return Err(format!(
                    "{name} is {n}; it must be at least {MIN_BUILD_WALL_MS} (the builder keeps a tenth of it as its timeout margin, and redaction needs time)"
                ));
            }
            if n == 0 {
                return Err(format!("{name} is 0; it must be at least 1"));
            }
            let (max, unit) = match name.ends_with("_ms") {
                true => (
                    MAX_DURATION_MS,
                    " (100 years, a PostgreSQL interval from now)",
                ),
                false => (i64::MAX as u64, ""),
            };
            if n > max {
                return Err(format!("{name} is {n}; it must be at most {max}{unit}"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_limit_must_fit_an_interval_from_now() {
        let mut limits = EnvironmentLimits::default();
        assert_eq!(limits.validate(), Ok(()));
        limits.build_wall_ms = MAX_DURATION_MS + 1;
        let refused = limits.validate().unwrap_err();
        assert!(
            refused.starts_with("build_wall_ms is 3155695200001; it must be at most"),
            "{refused}"
        );
        limits.build_wall_ms = MAX_DURATION_MS;
        limits.log_bytes = i64::MAX as u64 + 1;
        assert!(limits.validate().unwrap_err().starts_with("log_bytes is"));
        limits.log_bytes = 0;
        assert!(
            limits
                .validate()
                .unwrap_err()
                .starts_with("log_bytes is 0; it must be at least 1")
        );
    }

    #[test]
    fn build_wall_ms_below_the_minimum_is_refused() {
        let mut limits = EnvironmentLimits {
            build_wall_ms: MIN_BUILD_WALL_MS - 1,
            ..Default::default()
        };
        let refused = limits.validate().unwrap_err();
        assert!(
            refused.starts_with("build_wall_ms is 59999; it must be at least 60000"),
            "{refused}"
        );
        limits.build_wall_ms = MIN_BUILD_WALL_MS;
        assert_eq!(limits.validate(), Ok(()));
        limits.max_job_claims = 0;
        assert!(
            limits
                .validate()
                .unwrap_err()
                .starts_with("max_job_claims is 0")
        );
    }
}
