use super::{Git, GithubRepository, OnboardingConfig, Row, aq_git, load, response, slug};
use crate::source_checks::{observed_ref, repository_identity};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, types::Json};
use std::path::Path;
use uuid::Uuid;

/// Owner expectations for a project whose execution policy and central Git
/// binding were already seeded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptRequest {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub expected_binding_generation: u64,
    pub github: GithubRepository,
    pub remote_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AdoptionError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("project not found")]
    MissingProject,
    #[error("{0}")]
    Conflict(&'static str),
    #[error("{0} is unavailable")]
    Unavailable(&'static str),
    #[error("adoption database error")]
    Database(#[from] sqlx::Error),
}
use AdoptionError::{Conflict, Invalid, MissingProject, Unavailable};
type Result<T> = std::result::Result<T, AdoptionError>;

pub struct Adoption<'a> {
    pub pool: &'a PgPool,
    pub socket: Option<&'a Path>,
    pub onboarding: Option<&'a OnboardingConfig>,
    #[cfg(feature = "delivery_playbook")]
    pub github_app: Option<&'a crate::github::GitHubAppConfig>,
}

#[derive(Deserialize)]
struct Binding {
    project_id: Uuid,
    enabled: bool,
    repository_id: Uuid,
    repository_binding_generation: u64,
    repository_adapter_reference: String,
    publication: Option<crate::PublicationPolicy>,
}

impl Binding {
    fn check(&self, request: &AdoptRequest) -> Result<()> {
        if !self.enabled {
            return Err(Conflict("project binding is disabled"));
        }
        if self.project_id != request.project_id
            || self.repository_id != request.repository_id
            || self.repository_adapter_reference != "aq.git.configured"
            || self.repository_binding_generation != request.expected_binding_generation
        {
            return Err(Conflict("project repository binding or generation differs"));
        }
        Ok(())
    }

    fn check_target(&self, target: &str) -> Result<()> {
        if self
            .publication
            .as_ref()
            .is_some_and(|p| p.target != target)
        {
            return Err(Conflict(
                "default ref differs from the project publication target",
            ));
        }
        Ok(())
    }
}

async fn binding(conn: &mut PgConnection, project: Uuid) -> Result<Option<(i64, Binding)>> {
    let row = sqlx::query_as(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR UPDATE",
    )
    .bind(project)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(revision, Json(binding))| (revision, binding)))
}

impl AdoptRequest {
    fn validate(&self) -> Result<()> {
        if self.operation_id.is_nil()
            || self.project_id.is_nil()
            || self.repository_id.is_nil()
            || self.expected_binding_generation == 0
            || self.expected_binding_generation > i64::MAX as u64
            || !slug(&self.github.owner)
            || !slug(&self.github.name)
        {
            return Err(Invalid("invalid adoption identity"));
        }
        let name = format!("{}/{}", self.github.owner, self.github.name);
        if ![
            format!("https://github.com/{name}"),
            format!("https://github.com/{name}.git"),
            format!("ssh://git@github.com/{name}.git"),
        ]
        .contains(&self.remote_url)
        {
            return Err(Invalid(
                "remote_url must be an exact GitHub origin for the requested repository",
            ));
        }
        if self
            .default_branch
            .as_deref()
            .is_some_and(|b| super::branch_ref(b).is_none())
        {
            return Err(Invalid("invalid default branch"));
        }
        Ok(())
    }

    async fn existing(
        &self,
        conn: &mut PgConnection,
        actor: Uuid,
        digest: &str,
    ) -> Result<Option<Row>> {
        let row = load(conn, self.operation_id).await?;
        if row.as_ref().is_some_and(|r| {
            r.actor != actor
                || r.digest != digest
                || r.step != "bound"
                || r.state["adopted"] != true
        }) {
            return Err(Conflict("operation_id was used for a different request"));
        }
        Ok(row)
    }

    async fn check_history(&self, conn: &mut PgConnection) -> Result<()> {
        // An ordinary onboarding may have created the binding before recording
        // its final step. Its operation owns the project even in that window.
        let occupied: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM repository_onboardings
                WHERE project_id=$1 AND step<>'refused' AND (
                    state->'adopted' IS DISTINCT FROM 'true'::jsonb OR
                    (repository_id=$2 AND (step<>'bound' OR
                        state->'binding_generation' >= to_jsonb($3::bigint) OR
                        jsonb_typeof(state->'binding_generation') IS DISTINCT FROM 'number' OR
                        (state->>'binding_generation') !~ '^[1-9][0-9]*$'))
                )
            )",
        )
        .bind(self.project_id)
        .bind(self.repository_id)
        .bind(self.expected_binding_generation as i64)
        .fetch_one(conn)
        .await?;
        if occupied {
            return Err(Conflict(
                "project already has an onboarding or adoption for this binding",
            ));
        }
        Ok(())
    }
}

impl Adoption<'_> {
    pub async fn record(
        &self,
        actor: Uuid,
        digest: String,
        request: AdoptRequest,
        now_ms: impl FnOnce() -> i64 + Send,
    ) -> Result<Value> {
        request.validate()?;
        let (revision, project) = {
            let mut tx = self.pool.begin().await?;
            let current = binding(&mut tx, request.project_id).await?;
            let old = request.existing(&mut tx, actor, &digest).await?;
            let current = current.ok_or(MissingProject)?;
            current.1.check(&request)?;
            if let Some(old) = old {
                return replay(&current.1, &old);
            }
            request.check_history(&mut tx).await?;
            tx.commit().await?;
            current
        };

        let show = self.git_read(&request, "repo.show", json!({})).await?;
        let identity = repository_identity(
            &show,
            request.repository_id,
            request.expected_binding_generation,
        )
        .ok_or(Conflict(
            "aq-git did not report the expected central SHA-1 binding",
        ))?;
        if show["remote_url"] != request.remote_url.as_str() {
            return Err(Conflict("aq-git origin differs from remote_url"));
        }
        let branch = self.default_branch(&request).await?;
        let target =
            super::branch_ref(&branch).ok_or(Conflict("GitHub reported an invalid default ref"))?;
        project.check_target(&target)?;
        let permits = |field: &str, name: &str| {
            show[field]
                .as_array()
                .is_some_and(|refs| refs.iter().any(|r| r == name))
        };
        let can_publish = permits("allowed_targets", &target)
            || (project.publication.is_none()
                && permits("allowed_target_prefixes", "refs/heads/aq/"));
        if !permits("allowed_refs", &target) || !can_publish {
            return Err(Conflict(
                "aq-git must permit the default source ref and the project's publication path",
            ));
        }
        let observation = self
            .git_read(&request, "repo.observation", json!({"refs":[target]}))
            .await?;
        if observed_ref(&observation, &target, request.expected_binding_generation).is_none()
            || observation["availability"] != "available"
        {
            return Err(Conflict(
                "aq-git has no retained SHA-1 observation for the default ref",
            ));
        }

        let mut tx = self.pool.begin().await?;
        let (current_revision, current) = binding(&mut tx, request.project_id)
            .await?
            .ok_or(MissingProject)?;
        current.check(&request)?;
        if let Some(old) = request.existing(&mut tx, actor, &digest).await? {
            return replay(&current, &old);
        }
        if current_revision != revision {
            return Err(Conflict("project binding changed during adoption"));
        }
        request.check_history(&mut tx).await?;
        let value = json!({
            "request": request, "adopted": true, "remote_url": request.remote_url,
            "default_branch": branch, "default_ref": target, "repository_identity": identity,
            "repository_observation_id": observation["observation_id"],
            "binding_generation": request.expected_binding_generation,
        });
        let inserted = sqlx::query(
            "INSERT INTO repository_onboardings
             (operation_id,actor_id,request_digest,project_id,repository_id,step,state,created_at_ms,updated_at_ms)
             VALUES ($1,$2,$3,$4,$5,'bound',$6,$7,$7) ON CONFLICT(operation_id) DO NOTHING",
        )
        .bind(request.operation_id)
        .bind(actor)
        .bind(&digest)
        .bind(request.project_id)
        .bind(request.repository_id)
        .bind(&value)
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let old = request
                .existing(&mut tx, actor, &digest)
                .await?
                .ok_or(Conflict("operation_id was already used"))?;
            return replay(&current, &old);
        }
        tx.commit().await?;
        Ok(response(&Row {
            actor,
            digest,
            step: "bound".into(),
            state: value,
        }))
    }

    async fn git_read(
        &self,
        request: &AdoptRequest,
        operation: &'static str,
        params: Value,
    ) -> Result<Value> {
        match aq_git(
            self.socket,
            Uuid::new_v4(),
            operation,
            request.repository_id,
            request.expected_binding_generation,
            params,
        )
        .await
        {
            Git::Ok(value) => Ok(value),
            Git::Refused { code, .. }
                if code == "binding_conflict" || code == "repository_not_found" =>
            {
                Err(Conflict("aq-git repository binding or generation differs"))
            }
            Git::Refused { code, .. } if code == "observation_unavailable" => Err(Conflict(
                "aq-git has no retained observation for the default ref",
            )),
            Git::Refused { .. } | Git::Unavailable => Err(Unavailable(operation)),
        }
    }
}

fn replay(binding: &Binding, row: &Row) -> Result<Value> {
    let reference = row.state["default_ref"]
        .as_str()
        .ok_or(Conflict("adoption record has no verified default ref"))?;
    binding.check_target(reference)?;
    Ok(response(row))
}
