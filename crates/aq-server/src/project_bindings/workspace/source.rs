use super::*;
use aq_delivery::{
    offer_source::read,
    source_checks::{Repository, commit_matches, resolve_ref, retention_holds},
};

pub(super) async fn select(
    s: &AppState,
    home: Uuid,
    task: Uuid,
    target: &ProjectRecord,
    access: Access,
) -> Result<SourcePin> {
    let target_id = target.binding.project_id;
    let revision = target.revision;
    let previous:Option<String>=sqlx::query_scalar("SELECT commit_oid FROM task_workspace_sources WHERE home_project_id=$1 AND task_id=$2 AND target_project_id=$3 AND project_revision=$4")
        .bind(home).bind(task).bind(target_id).bind(revision as i64).fetch_optional(&s.store.pool).await.map_err(db)?;
    let commit = match previous {
        Some(oid) => oid,
        None => configured_head(s, target, task).await?,
    };
    let pin = SourcePin {
        project_id: target_id,
        project_revision: revision,
        repository_id: target.binding.repository_id,
        binding_generation: target.binding.repository_binding_generation,
        commit,
        access,
    };
    pin.validate(target).map_err(conflict)?;
    Ok(pin)
}
fn unavailable() -> AppError {
    conflict("workspace target configured source is unavailable")
}

async fn configured_head(s: &AppState, target: &ProjectRecord, task: Uuid) -> Result<String> {
    let cfg = s
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(unavailable)?;
    let project = target.binding.project_id;
    let policy = cfg
        .project_policy_record(project)
        .map_err(|_| unavailable())?;
    if !policy.enabled {
        return Err(unavailable());
    }
    let repo = Repository {
        id: target.binding.repository_id,
        generation: target.binding.repository_binding_generation,
    };
    let socket = &s
        .source_retention
        .as_ref()
        .ok_or_else(unavailable)?
        .config
        .git_socket;
    let (oid, handle) = match policy.source {
        aq_admission::SourceSelection::Immutable { object } => {
            (object.hex, policy.retention_handle)
        }
        aq_admission::SourceSelection::NamedRef { reference } => {
            named_head(
                s,
                socket,
                repo,
                project,
                task,
                reference.as_str(),
                policy.retention_handle,
            )
            .await?
        }
    };
    verify(socket, repo, &oid, handle).await?;
    Ok(oid)
}

async fn named_head(
    s: &AppState,
    socket: &std::path::Path,
    repo: Repository,
    project: Uuid,
    _task: Uuid,
    reference: &str,
    configured_handle: Uuid,
) -> Result<(String, Uuid)> {
    let now = || (s.clock)().timestamp_millis().max(0) as u64;
    #[cfg(feature = "delivery_playbook")]
    if crate::profiled_execution::delivery_enrolled(s, project, _task) {
        use aq_delivery::main_sync::{MainFollower, Scope};
        let scope = Scope::new(project, repo.id, repo.generation, reference);
        let current = MainFollower::new(&s.store.pool, socket, now)
            .current(&scope)
            .await
            .map_err(|_| unavailable())?;
        return Ok((current.head, current.retention_handle));
    }
    if let Some(published) = crate::profiled_execution::settlement::publication::current_head(
        &s.store.pool,
        project,
        repo.id,
        repo.generation,
        reference,
    )
    .await
    .map_err(|_| unavailable())?
    {
        return Ok((published.oid, published.retention_handle));
    }
    let observation = read(
        socket,
        repo,
        "repo.observation",
        json!({"refs":[reference],"max_age_ms":30000}),
    )
    .await
    .map_err(|_| unavailable())?;
    let oid =
        resolve_ref(&observation, reference, repo.generation, now()).ok_or_else(unavailable)?;
    Ok((oid, configured_handle))
}

async fn verify(socket: &std::path::Path, repo: Repository, oid: &str, handle: Uuid) -> Result<()> {
    let inspection = read(
        socket,
        repo,
        "inspect",
        json!({"oid":{"algorithm":"sha1","hex":oid}}),
    )
    .await
    .map_err(|_| unavailable())?;
    if !commit_matches(&inspection, oid) {
        return Err(unavailable());
    }
    let retention = read(
        socket,
        repo,
        "retention.inspect",
        json!({"handle_id":handle,"limit":1}),
    )
    .await
    .map_err(|_| unavailable())?;
    if !retention_holds(&retention, handle, repo, oid) {
        return Err(unavailable());
    }
    Ok(())
}
