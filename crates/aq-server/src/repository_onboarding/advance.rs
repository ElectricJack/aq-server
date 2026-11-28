use super::*;
use std::ops::ControlFlow;

struct Run<'a> {
    state: &'a AppState,
    config: &'a OnboardingConfig,
    request: &'a OnboardRequest,
    actor: Uuid,
}

pub(super) async fn advance(
    state: &AppState,
    actor: Uuid,
    request: &OnboardRequest,
    row: &mut Row,
) -> Result<()> {
    let config = state.config.onboarding.as_ref().ok_or(AppError::Internal)?;
    let run = Run {
        state,
        config,
        request,
        actor,
    };
    loop {
        let transition = match row.step.as_str() {
            "started" => run.register(row).await?,
            "registered" => run.sync(row).await,
            "synced" => run.tree(row).await,
            "checked" => run.bind(row).await?,
            _ => return Ok(()),
        };
        let next = transition.apply(row);
        save(state, request.operation_id, &row.step, &row.state).await?;
        if next.is_break() {
            return Ok(());
        }
    }
}
impl Run<'_> {
    async fn git(&self, operation: &str, generation: u64, params: Value) -> Git {
        wire::aq_git(
            git_socket(self.state),
            aq_delivery::OperationContext {
                operation_id: step_operation(self.request.operation_id, operation),
                repository_id: self.request.repository_id,
                binding_generation: generation,
            },
            operation,
            params,
        )
        .await
    }

    async fn token(&self) -> std::result::Result<Option<String>, Transition> {
        let profile = self
            .request
            .credential_profile
            .as_ref()
            .and_then(|name| self.config.credential_profiles.get(name));
        let Some(p) = profile.filter(|p| p.kind == CredentialKind::HttpsFineGrainedToken) else {
            return Ok(None);
        };
        tokio::fs::read_to_string(&p.secret_file)
            .await
            .map(|t| Some(t.trim().to_string()))
            .map_err(|_| {
                Transition::retry(
                    "credential_unreadable",
                    "vault credential profile is unreadable",
                )
            })
    }

    async fn branch(&self, row: &mut Row) -> Result<ControlFlow<Transition, String>> {
        if let Some(branch) = row.state["default_branch"].as_str() {
            return Ok(ControlFlow::Continue(branch.into()));
        }
        let token = match self.token().await {
            Ok(t) => t,
            Err(stop) => return Ok(ControlFlow::Break(stop)),
        };
        let github = Github {
            config: self.config,
            token,
        };
        let (branch, signing) = match github
            .metadata(&self.request.github, self.request.default_branch.as_deref())
            .await
        {
            Meta::Branch { branch, signing } => (branch, signing),
            Meta::Unavailable(e) => {
                return Ok(ControlFlow::Break(Transition::retry(
                    "github_metadata_unavailable",
                    &e,
                )));
            }
        };
        row.state["default_branch"] = json!(branch);
        if signing {
            return Ok(ControlFlow::Break(Transition::signing_required()));
        }
        // The template is re-validated with the real branch.
        binding(self.request, 1, &branch)?;
        save(self.state, self.request.operation_id, "started", &row.state).await?;
        Ok(ControlFlow::Continue(branch))
    }

    async fn register(&self, row: &mut Row) -> Result<Transition> {
        let branch = match self.branch(row).await? {
            ControlFlow::Break(stop) => return Ok(stop),
            ControlFlow::Continue(branch) => branch,
        };
        let target = format!("refs/heads/{branch}");
        let mut params = json!({"identity":format!("github.com/{}/{}", self.request.github.owner, self.request.github.name),
                    "mode":"central","remote_url":row.state["remote_url"],"allowed_refs":[target],"allowed_targets":[target]});
        if let Some(name) = &self.request.credential_profile {
            params["credential_profile"] = json!(name);
        }
        // Spec 0016 §5.2: each task's branch `refs/heads/aq/<task-id>`.
        {
            params["allowed_target_prefixes"] = json!(["refs/heads/aq/"]);
        }
        let reply = self.git("repo.register", 0, params).await;
        Ok(Transition::registration(
            reply,
            self.request.credential_profile.is_some(),
        ))
    }

    async fn sync(&self, row: &Row) -> Transition {
        let target = format!(
            "refs/heads/{}",
            row.state["default_branch"].as_str().unwrap_or("")
        );
        let reply = self
            .git("repo.sync", generation(row), json!({"refs":[target]}))
            .await;
        Transition::sync(reply, &target)
    }
    async fn tree(&self, row: &Row) -> Transition {
        let oid = json!({"algorithm":"sha1","hex":row.state["base"]["oid"]});
        Transition::tree(
            self.git("tree", generation(row), json!({"oid":oid,"limit":1000}))
                .await,
        )
    }
    async fn bind(&self, row: &Row) -> Result<Transition> {
        let binding = binding(
            self.request,
            generation(row),
            row.state["default_branch"].as_str().unwrap_or(""),
        )?;
        let reply = crate::project_bindings::put_binding(
            self.state,
            self.actor,
            self.request.operation_id,
            0,
            binding,
        )
        .await;
        Ok(match reply {
            Ok(project) => Transition::Bound(project),
            Err(AppError::Conflict(m) | AppError::BadRequest(m)) => {
                Transition::retry("project_binding_refused", &m)
            }
            Err(_) => Transition::retry("project_binding_unavailable", "project binding failed"),
        })
    }
}

fn generation(row: &Row) -> u64 {
    row.state["binding_generation"].as_u64().unwrap_or(0)
}
