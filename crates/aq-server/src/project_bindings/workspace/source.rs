use super::*;
use aq_delivery::{
    offer_source::read,
    source_checks::{Repository, commit_matches, resolve_ref},
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
        None => configured_head(s, target).await?,
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
async fn configured_head(s: &AppState, target: &ProjectRecord) -> Result<String> {
    let missing = || conflict("workspace target configured source is unavailable");
    let cfg = s.config.profiled_execution.as_ref().ok_or_else(missing)?;
    let policy = cfg
        .project_policy_record(target.binding.project_id)
        .map_err(|_| missing())?;
    if !policy.enabled {
        return Err(missing());
    }
    let repo = Repository {
        id: target.binding.repository_id,
        generation: target.binding.repository_binding_generation,
    };
    let socket = &s
        .source_retention
        .as_ref()
        .ok_or_else(missing)?
        .config
        .git_socket;
    let oid = match policy.source {
        aq_admission::SourceSelection::Immutable { object } => object.hex,
        aq_admission::SourceSelection::NamedRef { reference } => {
            let observation = read(
                socket,
                repo,
                "repo.observation",
                json!({"refs":[reference.as_str()],"max_age_ms":30000}),
            )
            .await
            .map_err(|_| missing())?;
            resolve_ref(
                &observation,
                reference.as_str(),
                repo.generation,
                (s.clock)().timestamp_millis().max(0) as u64,
            )
            .ok_or_else(missing)?
        }
    };
    let inspection = read(
        socket,
        repo,
        "inspect",
        json!({"oid":{"algorithm":"sha1","hex":oid}}),
    )
    .await
    .map_err(|_| missing())?;
    if !commit_matches(&inspection, &oid) {
        return Err(missing());
    }
    Ok(oid)
}
