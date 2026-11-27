use super::{
    Git, GithubRepository, OnboardingConfig, RequestOwner, Row, aq_git, load, response, slug,
};
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
mod recording;
use recording::{Attempt, VerifiedAdoption};

type Result<T> = std::result::Result<T, AdoptionError>;

pub struct Adoption<'a> {
    pub pool: &'a PgPool,
    pub socket: Option<&'a Path>,
    pub onboarding: Option<&'a OnboardingConfig>,
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
        let expected = (
            request.project_id,
            request.repository_id,
            request.expected_binding_generation,
            "aq.git.configured",
        );
        let configured = (
            self.project_id,
            self.repository_id,
            self.repository_binding_generation,
            self.repository_adapter_reference.as_str(),
        );
        if configured != expected {
            return Err(Conflict("project repository binding or generation differs"));
        }
        Ok(())
    }

    fn check_permissions(&self, show: &Value, target: &str) -> Result<()> {
        let permits = |field: &str, name: &str| {
            show[field]
                .as_array()
                .is_some_and(|refs| refs.iter().any(|r| r == name))
        };
        let can_publish = permits("allowed_targets", target)
            || (self.publication.is_none() && permits("allowed_target_prefixes", "refs/heads/aq/"));
        if !permits("allowed_refs", target) || !can_publish {
            return Err(Conflict(
                "aq-git must permit the default source ref and the project's publication path",
            ));
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
        let valid_ids = [self.operation_id, self.project_id, self.repository_id]
            .iter()
            .all(|id| !id.is_nil());
        let valid_generation = (1..=i64::MAX as u64).contains(&self.expected_binding_generation);
        let valid_repository = slug(&self.github.owner) && slug(&self.github.name);
        if ![valid_ids, valid_generation, valid_repository]
            .into_iter()
            .all(|valid| valid)
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
        owner: RequestOwner<'_>,
    ) -> Result<Option<Row>> {
        let row = load(conn, self.operation_id).await?;
        if row.as_ref().is_some_and(|r| {
            r.actor != owner.actor
                || r.digest != owner.digest
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
        owner: RequestOwner<'_>,
        request: AdoptRequest,
        now_ms: impl FnOnce() -> i64 + Send,
    ) -> Result<Value> {
        request.validate()?;
        let attempt = Attempt {
            request: &request,
            owner,
        };
        let (revision, project) = match attempt.inspect(self.pool).await? {
            std::ops::ControlFlow::Continue(binding) => binding,
            std::ops::ControlFlow::Break(replay) => return Ok(replay),
        };
        let proof = self.observe(&request, &project, revision).await?;
        attempt.commit(self.pool, proof, now_ms).await
    }

    async fn observe(
        &self,
        request: &AdoptRequest,
        project: &Binding,
        revision: i64,
    ) -> Result<VerifiedAdoption> {
        let show = self.git_read(request, "repo.show", json!({})).await?;
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
        let branch = self.default_branch(request).await?;
        let target =
            super::branch_ref(&branch).ok_or(Conflict("GitHub reported an invalid default ref"))?;
        project.check_target(&target)?;
        project.check_permissions(&show, &target)?;
        let observation = self
            .git_read(request, "repo.observation", json!({"refs":[target]}))
            .await?;
        if observed_ref(&observation, &target, request.expected_binding_generation).is_none()
            || observation["availability"] != "available"
        {
            return Err(Conflict(
                "aq-git has no retained SHA-1 observation for the default ref",
            ));
        }

        let value = json!({
            "request": request, "adopted": true, "remote_url": request.remote_url,
            "default_branch": branch, "default_ref": target, "repository_identity": identity,
            "repository_observation_id": observation["observation_id"],
            "binding_generation": request.expected_binding_generation,
        });
        Ok(VerifiedAdoption {
            binding_revision: revision,
            state: value,
        })
    }

    async fn git_read(
        &self,
        request: &AdoptRequest,
        operation: &'static str,
        params: Value,
    ) -> Result<Value> {
        match aq_git(
            self.socket,
            crate::OperationContext {
                operation_id: Uuid::new_v4(),
                repository_id: request.repository_id,
                binding_generation: request.expected_binding_generation,
            },
            operation,
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
