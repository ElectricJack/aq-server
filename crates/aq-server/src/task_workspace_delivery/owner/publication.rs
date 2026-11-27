use super::*;
use aq_delivery::commands::{
    Delivery, DeliveryTarget, GitTarget, OpenPullRequest, Step, TaskOperation, onboarded_target,
};

pub(super) async fn configured(
    state: &AppState,
    source: &Source,
    db: &mut PgConnection,
) -> Result<(Delivery, DeliveryTarget), AppError> {
    let Some(target_ref) = source
        .target_ref
        .as_deref()
        .filter(|_| source.publication_mode.as_deref() == Some("pull_request"))
    else {
        return Err(conflict(
            "target does not have an installed pull request policy",
        ));
    };
    let current:Option<i64>=sqlx::query_scalar(
        "SELECT revision FROM project_execution_bindings WHERE project_id=$1 AND revision=$2 FOR SHARE",
    ).bind(source.project_id).bind(source.project_revision).fetch_optional(db).await.map_err(crate::error::db)?;
    if current.is_none() {
        return Err(conflict("target publication policy changed"));
    }
    let git = GitTarget {
        repository_id: source.repository_id,
        binding_generation: u64::try_from(source.binding_generation)
            .map_err(|_| AppError::Internal)?,
    };
    let target = onboarded_target(&state.store.pool, source.project_id, git)
        .await
        .map_err(|_| AppError::UpdatesUnavailable("target onboarding unavailable".into()))?
        .filter(|target| format!("refs/heads/{}", target.default_branch) == target_ref)
        .ok_or_else(|| conflict("onboarded target differs from the configured branch"))?;
    let socket = &state
        .source_retention
        .as_ref()
        .ok_or_else(|| AppError::UpdatesUnavailable("central Git unavailable".into()))?
        .config
        .git_socket;
    let forge = state
        .config
        .delivery
        .as_ref()
        .and_then(|settings| settings.github.as_ref());
    let (limits, _) = aq_role_settings::delivery_limits(&state.store.pool)
        .await
        .map_err(|_| AppError::UpdatesUnavailable("delivery limits unavailable".into()))?;
    let wait = std::time::Duration::from_millis(limits.git_transfer_timeout_ms);
    Ok((
        Delivery::configured(socket, forge).git_waiting(socket, wait),
        target,
    ))
}

pub(super) struct Admission<'a> {
    pub(super) subject: Subject,
    pub(super) plan: &'a Plan,
    pub(super) source: &'a Source,
    pub(super) operation: Uuid,
    pub(super) actor: &'a ActorPin,
}

pub(super) async fn publish(
    state: &AppState,
    mut tx: Transaction<'_, Postgres>,
    admission: Admission<'_>,
) -> Result<Value, AppError> {
    let Admission {
        subject,
        plan,
        source,
        operation,
        actor,
    } = admission;
    if target_accepted(&mut tx, plan, source).await? {
        tx.commit().await.map_err(crate::error::db)?;
        return Ok(json!({"state":"already_accepted"}));
    }
    let (delivery, target) = configured(state, source, &mut tx).await?;
    let prior:Option<String>=sqlx::query_scalar(
        "SELECT result_head FROM task_workspace_publications WHERE home_project_id=$1 AND task_id=$2
         AND target_project_id=$3 AND repository_id=$4 AND binding_generation=$5 AND published_at_ms IS NOT NULL
         ORDER BY published_at_ms DESC,operation_id DESC LIMIT 1",
    ).bind(plan.home).bind(plan.task).bind(source.project_id).bind(source.repository_id).bind(source.binding_generation)
     .fetch_optional(&mut *tx).await.map_err(crate::error::db)?;
    let expected_old = if prior.is_some()
        && delivery
            .publish_lease_usable(&target, plan.task, &source.head, None)
            .await
    {
        None
    } else {
        prior
    };
    let at = now(state)?;
    sqlx::query(
        "INSERT INTO task_workspace_publications(operation_id,home_project_id,task_id,target_project_id,project_revision,
         repository_id,binding_generation,result_head,target_ref,expected_old,actor_id,created_at_ms)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) ON CONFLICT DO NOTHING",
    ).bind(operation).bind(plan.home).bind(plan.task).bind(source.project_id).bind(source.project_revision)
     .bind(source.repository_id).bind(source.binding_generation).bind(&source.head).bind(&source.target_ref)
     .bind(expected_old).bind(actor.id()).bind(at).execute(&mut *tx).await.map_err(crate::error::db)?;
    let active:Uuid=sqlx::query_scalar(
        "SELECT operation_id FROM task_workspace_publications WHERE home_project_id=$1 AND task_id=$2 AND target_project_id=$3
         AND project_revision=$4 AND repository_id=$5 AND binding_generation=$6 AND result_head=$7 AND target_ref=$8",
    ).bind(plan.home).bind(plan.task).bind(source.project_id).bind(source.project_revision).bind(source.repository_id)
     .bind(source.binding_generation).bind(&source.head).bind(&source.target_ref)
     .fetch_optional(&mut *tx).await.map_err(crate::error::db)?
     .ok_or_else(||conflict("workspace publication operation ID already used"))?;
    tx.commit().await.map_err(crate::error::db)?;
    let pending = Pending {
        subject,
        project: source.project_id,
        head: &source.head,
        operation: active,
        actor,
    };
    publish_branch(state, &pending).await?;
    open_pull_request(state, &pending).await
}

struct Pending<'a> {
    subject: Subject,
    project: Uuid,
    head: &'a str,
    operation: Uuid,
    actor: &'a ActorPin,
}

impl Pending<'_> {
    async fn fence(
        &self,
        state: &AppState,
        db: &mut Transaction<'_, Postgres>,
    ) -> Result<(Delivery, DeliveryTarget), AppError> {
        let plan = plan(state, db, self.subject, true).await?;
        self.actor.fence(db).await?;
        let source = source(&plan, self.project, self.head)?;
        configured(state, source, db).await
    }
}

async fn publish_branch(state: &AppState, pending: &Pending<'_>) -> Result<(), AppError> {
    let mut tx = state.store.pool.begin().await.map_err(crate::error::db)?;
    let (delivery, target) = pending.fence(state, &mut tx).await?;
    let (published,previous):(Option<i64>,Option<String>)=sqlx::query_as(
        "SELECT published_at_ms,expected_old FROM task_workspace_publications WHERE operation_id=$1 FOR UPDATE",
    ).bind(pending.operation).fetch_one(&mut *tx).await.map_err(crate::error::db)?;
    if published.is_none() {
        let result = delivery
            .branch_publish(
                TaskOperation::new(pending.operation, &target, pending.subject.task),
                previous.as_deref(),
                Some(pending.head),
            )
            .await;
        if !matches!(result,Step::Done(done) if done.outcome=="published" && done.output["head"]==pending.head)
        {
            return Err(conflict(
                "workspace branch publication did not confirm the retained head",
            ));
        }
        sqlx::query(
            "UPDATE task_workspace_publications SET published_at_ms=$2 WHERE operation_id=$1",
        )
        .bind(pending.operation)
        .bind(now(state)?)
        .execute(&mut *tx)
        .await
        .map_err(crate::error::db)?;
    }
    tx.commit().await.map_err(crate::error::db)?;
    Ok(())
}

async fn open_pull_request(state: &AppState, pending: &Pending<'_>) -> Result<Value, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(crate::error::db)?;
    let (delivery, target) = pending.fence(state, &mut tx).await?;
    let (number,url):(Option<i64>,Option<String>)=sqlx::query_as(
        "SELECT pull_request,pull_request_url FROM task_workspace_publications WHERE operation_id=$1 FOR UPDATE",
    ).bind(pending.operation).fetch_one(&mut *tx).await.map_err(crate::error::db)?;
    let (number, url) = if let (Some(number), Some(url)) = (number, url) {
        (number, url)
    } else {
        let title = format!("AQ task {}", pending.subject.task);
        let body = format!(
            "Retained workspace change for Home task {} at {}.",
            pending.subject.task, pending.head
        );
        let result = delivery
            .pull_request_open(
                &target,
                pending.subject.task,
                OpenPullRequest {
                    title: &title,
                    body: &body,
                    known_pr: None,
                },
            )
            .await;
        let Step::Done(done) = result else {
            return Err(conflict("workspace pull request is not available yet"));
        };
        let known = done
            .pull_request
            .ok_or_else(|| conflict("workspace pull request is not available yet"))?;
        let observed = delivery
            .pull_request_at(&target, known.number, pending.head)
            .await
            .map_err(|_| AppError::UpdatesUnavailable("target forge unavailable".into()))?
            .ok_or_else(|| {
                conflict("workspace pull request does not have the retained head and target")
            })?;
        let number = i64::try_from(observed.number).map_err(|_| AppError::Internal)?;
        if !(1..=1_000_000_000).contains(&number) || observed.url.len() > 2048 {
            return Err(AppError::UpdatesUnavailable(
                "target forge returned an invalid pull request".into(),
            ));
        }
        sqlx::query("UPDATE task_workspace_publications SET pull_request=$2,pull_request_url=$3,opened_at_ms=$4 WHERE operation_id=$1")
            .bind(pending.operation).bind(number).bind(&observed.url).bind(now(state)?)
            .execute(&mut *tx).await.map_err(crate::error::db)?;
        (number, observed.url)
    };
    tx.commit().await.map_err(crate::error::db)?;
    Ok(json!({"state":"pull_request","number":number,"url":url}))
}
