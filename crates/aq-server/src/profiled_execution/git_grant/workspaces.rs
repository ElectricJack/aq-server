//! The node requests only an attempt. Project, repository, commit and access
//! come from the approved task workspace and its current target binding.
use super::*;
use aq_delivery::commands::{GitTarget, object_id, task_branch};
use axum::{Json, Router, http::HeaderMap};
use serde::Serialize;
use sqlx::{FromRow, PgConnection, PgPool, Row, postgres::PgRow};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

const MAX_WORKSPACES: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Source {
    home_project_id: Uuid,
    task_id: Uuid,
    target_project_id: Uuid,
    project_revision: i64,
    repository_id: Uuid,
    binding_generation: i64,
    commit_oid: String,
    access: String,
}

#[derive(Serialize)]
pub(crate) struct WorkspaceGrant {
    pub(crate) project_id: Uuid,
    pub(crate) project_revision: u64,
    pub(crate) binding_generation: u64,
    pub(crate) access: String,
    pub(crate) checkout_head: String,
    #[serde(flatten)]
    pub(crate) grant: GitGrant,
}

#[derive(Serialize)]
struct GrantList {
    workspaces: Vec<WorkspaceGrant>,
    services: Vec<Service>,
}

#[derive(Serialize)]
struct Service {
    service: String,
}

struct Attempt {
    home_project_id: Uuid,
    task_id: Uuid,
    home_repository: Uuid,
    generation: i64,
}

struct Frozen {
    source: Source,
    task_ref: String,
    checkout_head: String,
    lease: Option<String>,
}

impl FromRow<'_, PgRow> for Source {
    fn from_row(row: &PgRow) -> sqlx::Result<Self> {
        Ok(Self {
            home_project_id: row.try_get("home_project_id")?,
            task_id: row.try_get("task_id")?,
            target_project_id: row.try_get("target_project_id")?,
            project_revision: row.try_get("project_revision")?,
            repository_id: row.try_get("repository_id")?,
            binding_generation: row.try_get("binding_generation")?,
            commit_oid: row.try_get("commit_oid")?,
            access: row.try_get("access")?,
        })
    }
}

impl FromRow<'_, PgRow> for Attempt {
    fn from_row(row: &PgRow) -> sqlx::Result<Self> {
        Ok(Self {
            home_project_id: row.try_get("home_project_id")?,
            task_id: row.try_get("task_id")?,
            home_repository: row.try_get("home_repository")?,
            generation: row.try_get("generation")?,
        })
    }
}

impl FromRow<'_, PgRow> for Frozen {
    fn from_row(row: &PgRow) -> sqlx::Result<Self> {
        Ok(Self {
            source: Source::from_row(row)?,
            task_ref: row.try_get("task_ref")?,
            checkout_head: row.try_get("checkout_head")?,
            lease: row.try_get("lease")?,
        })
    }
}

impl Frozen {
    fn into_wire(self) -> WorkspaceGrant {
        let s = self.source;
        WorkspaceGrant {
            project_id: s.target_project_id,
            project_revision: s.project_revision as u64,
            binding_generation: s.binding_generation as u64,
            access: s.access,
            checkout_head: self.checkout_head,
            grant: GitGrant {
                endpoint: format!(
                    "/v1/git/workspaces/{}/{}",
                    s.target_project_id, s.repository_id
                ),
                repository_id: s.repository_id,
                base: s.commit_oid,
                task_ref: self.task_ref,
                lease: self.lease,
                update_from_main: false,
            },
        }
    }
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/git/grants/{attempt}/workspaces", get(list))
        .route(
            "/v1/git/workspaces/{project}/{repository}/{*path}",
            any(proxy),
        )
}

async fn attempt_locked(
    db: &mut PgConnection,
    attempt: Uuid,
    node: Uuid,
) -> sqlx::Result<Option<Attempt>> {
    sqlx::query_as(include_str!("workspaces/attempt.sql"))
        .bind(attempt)
        .bind(node)
        .fetch_optional(db)
        .await
}

async fn sources_locked(db: &mut PgConnection, id: Uuid, a: &Attempt) -> sqlx::Result<Vec<Source>> {
    sqlx::query_as(include_str!("workspaces/sources.sql"))
        .bind(a.home_project_id)
        .bind(a.task_id)
        .bind(id)
        .bind(MAX_WORKSPACES as i64 + 1)
        .fetch_all(db)
        .await
}

async fn frozen_locked(db: &mut PgConnection, id: Uuid) -> sqlx::Result<Vec<Frozen>> {
    sqlx::query_as(
        "SELECT g.* FROM task_workspace_git_grants g
         WHERE g.attempt_id=$1 ORDER BY g.target_project_id FOR SHARE",
    )
    .bind(id)
    .fetch_all(db)
    .await
}

fn unavailable() -> AppError {
    AppError::Maintenance("task workspace git unavailable".into())
}

fn unauthorized() -> AppError {
    AppError::Forbidden("no live workspace git authority".into())
}

fn stale() -> AppError {
    AppError::Conflict("task workspace source or task branch changed".into())
}

async fn checked_scope(
    db: &mut PgConnection,
    id: Uuid,
    attempt: &Attempt,
) -> Result<(Vec<Source>, Vec<String>), AppError> {
    let sources = sources_locked(db, id, attempt)
        .await
        .map_err(crate::error::db)?;
    let services: Vec<String> = sqlx::query_scalar(
        "SELECT s.service FROM task_workspace_services s
         JOIN task_workspace_offer_fences f ON f.attempt_id=$3
         WHERE s.home_project_id=$1 AND s.task_id=$2 AND s.requested_attempt_id IS DISTINCT FROM $3
           AND s.service='postgres' AND f.scope->'services' ? s.service
         ORDER BY s.service FOR SHARE OF s",
    )
    .bind(attempt.home_project_id)
    .bind(attempt.task_id)
    .bind(id)
    .fetch_all(&mut *db)
    .await
    .map_err(crate::error::db)?;
    let scope: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT scope FROM task_workspace_offer_fences WHERE attempt_id=$1")
            .bind(id)
            .fetch_optional(&mut *db)
            .await
            .map_err(crate::error::db)?;
    if let Some(scope) = scope {
        if scope["sources"].as_array().map(Vec::len) != Some(sources.len())
            || scope["services"].as_array().map(Vec::len) != Some(services.len())
        {
            return Err(stale());
        }
    } else {
        let workspace_required: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM profiled_launches WHERE attempt_id=$1
                AND graph_task_observation #>> '{native_offer,workspace_required}'='true')",
        )
        .bind(id)
        .fetch_one(db)
        .await
        .map_err(crate::error::db)?;
        if workspace_required {
            return Err(stale());
        }
    }
    Ok((sources, services))
}

async fn list(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<GrantList>, AppError> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    let pool = &state.store.pool;
    let mut tx = pool.begin().await.map_err(crate::error::db)?;
    crate::worker_api::read_current_node(&mut tx, &node).await?;
    let a = attempt_locked(&mut tx, id, node.id())
        .await
        .map_err(crate::error::db)?
        .ok_or_else(unauthorized)?;
    let (sources, services) = checked_scope(&mut tx, id, &a).await?;
    let frozen = frozen_locked(&mut tx, id).await.map_err(crate::error::db)?;
    tx.commit().await.map_err(crate::error::db)?;

    let mut existing: BTreeMap<_, _> = frozen
        .into_iter()
        .map(|g| (g.source.target_project_id, g))
        .collect();
    let mut repos = BTreeSet::from([a.home_repository]);
    if sources.len() > MAX_WORKSPACES || !sources.iter().all(|s| repos.insert(s.repository_id)) {
        return Err(AppError::Conflict(
            "duplicate repository or workspace limit reached".into(),
        ));
    }
    let mut new_sources = Vec::new();
    for source in &sources {
        if let Some(old) = existing.remove(&source.target_project_id) {
            if old.source != *source {
                return Err(stale());
            }
            continue;
        }
        new_sources.push(source.clone());
    }

    let lookups = async {
        let mut discovered = Vec::with_capacity(new_sources.len());
        for source in new_sources {
            discovered.push(freeze(&state, source, a.generation).await?);
        }
        Ok::<_, AppError>(discovered)
    };
    let enrollments = tokio::time::timeout(Duration::from_secs(10), lookups)
        .await
        .map_err(|_| unavailable())??;

    let mut tx = pool.begin().await.map_err(crate::error::db)?;
    // The node lock orders this attempt's concurrent enrollments.
    crate::worker_api::lock_current_node(&mut tx, &node).await?;
    let a = attempt_locked(&mut tx, id, node.id())
        .await
        .map_err(crate::error::db)?
        .ok_or_else(unauthorized)?;
    if checked_scope(&mut tx, id, &a).await? != (sources.clone(), services.clone()) {
        return Err(stale());
    }
    for g in enrollments {
        record(&mut tx, id, &g).await.map_err(crate::error::db)?;
    }
    let current = sources
        .into_iter()
        .map(|s| (s.target_project_id, s))
        .collect::<BTreeMap<_, _>>();
    let workspaces = frozen_locked(&mut tx, id)
        .await
        .map_err(crate::error::db)?
        .into_iter()
        .filter(|g| current.get(&g.source.target_project_id) == Some(&g.source))
        .map(Frozen::into_wire)
        .collect();
    tx.commit().await.map_err(crate::error::db)?;
    Ok(Json(GrantList {
        workspaces,
        services: services
            .into_iter()
            .map(|service| Service { service })
            .collect(),
    }))
}

fn git(state: &AppState) -> Result<SocketGit, AppError> {
    let retention = state.source_retention.as_ref().ok_or_else(unavailable)?;
    Ok(SocketGit::new(&retention.config.git_socket))
}

async fn freeze(state: &AppState, source: Source, generation: i64) -> Result<Frozen, AppError> {
    if !object_id(&source.commit_oid) || source.commit_oid == "0".repeat(40) {
        return Err(stale());
    }
    if socket(state).is_none() {
        return Err(unavailable());
    }
    let git = git(state)?;
    let task_ref = task_branch(source.task_id);
    let (checkout_head, lease) = if source.access == "write" {
        let target = GitTarget {
            repository_id: source.repository_id,
            binding_generation: source.binding_generation as u64,
        };
        let previous = previous_head(&state.store.pool, &source, generation)
            .await
            .map_err(crate::error::db)?;
        let central = git
            .show_ref(&target, &task_ref)
            .await
            .map_err(|_| unavailable())?;
        checkout(&source.commit_oid, previous, central)?
    } else {
        (source.commit_oid.clone(), None)
    };
    Ok(Frozen {
        source,
        task_ref,
        checkout_head,
        lease,
    })
}

async fn previous_head(
    pool: &PgPool,
    source: &Source,
    generation: i64,
) -> sqlx::Result<Option<Option<String>>> {
    sqlx::query_scalar(include_str!("workspaces/previous_head.sql"))
        .bind(source.home_project_id)
        .bind(source.task_id)
        .bind(source.target_project_id)
        .bind(source.project_revision)
        .bind(source.repository_id)
        .bind(source.binding_generation)
        .bind(&source.commit_oid)
        .bind(generation)
        .fetch_optional(pool)
        .await
}

fn checkout(
    original: &str,
    previous: Option<Option<String>>,
    central: Option<String>,
) -> Result<(String, Option<String>), AppError> {
    let expected = match previous {
        Some(Some(head)) if head.len() == 40 && object_id(&head) => head,
        None => original.to_owned(),
        Some(_) => return Err(stale()),
    };
    if central.as_deref() == Some(expected.as_str()) || (central.is_none() && expected == original)
    {
        Ok((expected, central))
    } else {
        Err(stale())
    }
}

async fn record(db: &mut PgConnection, attempt: Uuid, frozen: &Frozen) -> sqlx::Result<()> {
    let s = &frozen.source;
    sqlx::query(
        "INSERT INTO task_workspace_git_grants
         (attempt_id,home_project_id,task_id,target_project_id,project_revision,repository_id,
          binding_generation,commit_oid,access,task_ref,checkout_head,lease)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
         ON CONFLICT(attempt_id,target_project_id) DO NOTHING",
    )
    .bind(attempt)
    .bind(s.home_project_id)
    .bind(s.task_id)
    .bind(s.target_project_id)
    .bind(s.project_revision)
    .bind(s.repository_id)
    .bind(s.binding_generation)
    .bind(&s.commit_oid)
    .bind(&s.access)
    .bind(&frozen.task_ref)
    .bind(&frozen.checkout_head)
    .bind(&frozen.lease)
    .execute(db)
    .await?;
    Ok(())
}

pub(crate) async fn live_grant_locked(
    db: &mut PgConnection,
    attempt: Uuid,
    node: Uuid,
    project: Uuid,
) -> sqlx::Result<Option<WorkspaceGrant>> {
    let Some(a) = attempt_locked(db, attempt, node).await? else {
        return Ok(None);
    };
    let row: Option<Frozen> = sqlx::query_as(include_str!("workspaces/live.sql"))
        .bind(attempt)
        .bind(project)
        .bind(a.home_project_id)
        .bind(a.task_id)
        .fetch_optional(db)
        .await?;
    Ok(row.map(Frozen::into_wire))
}

async fn proxy(
    State(state): State<AppState>,
    Path((project, repository, path)): Path<(Uuid, Uuid, String)>,
    request: Request,
) -> Result<Response, AppError> {
    let node = crate::worker_api::authenticate_context(&state, request.headers()).await?;
    let id = request
        .headers()
        .get(aq_delivery::git_grant::ATTEMPT_HEADER)
        .and_then(|v| Uuid::try_parse(v.to_str().ok()?).ok())
        .ok_or_else(|| {
            AppError::BadRequest("the AQ-Attempt header must name the attempt".into())
        })?;
    let pool: &PgPool = &state.store.pool;
    let mut tx = pool.begin().await.map_err(crate::error::db)?;
    crate::worker_api::read_current_node(&mut tx, &node).await?;
    let grant = live_grant_locked(&mut tx, id, node.id(), project)
        .await
        .map_err(crate::error::db)?
        .filter(|g| g.grant.repository_id == repository)
        .ok_or_else(unauthorized)?;
    let socket = socket(&state).ok_or_else(unavailable)?;
    let query = request.uri().query();
    if path == "aq-source-bundle" {
        let offset = (request.method() == axum::http::Method::GET)
            .then(|| query?.strip_prefix("offset=")?.parse::<u64>().ok())
            .flatten()
            .ok_or_else(|| AppError::BadRequest("invalid workspace bundle range".into()))?;
        // Release assignment locks before export I/O, then recheck authority
        // before returning these bounded bytes to the node.
        tx.commit().await.map_err(crate::error::db)?;
        return bundle(&state, &node, id, grant, offset).await;
    }
    let push =
        aq_delivery::git_grant::route(request.method(), &path, query).map_err(super::refusal)?;
    if grant.access != "write" && (push || query == Some("service=git-receive-pack")) {
        return Err(AppError::Forbidden("workspace is read only".into()));
    }
    // Central aq-git's command socket validates the exact repository binding
    // generation. Its smart HTTP path itself only names the repository ID.
    let target = GitTarget {
        repository_id: repository,
        binding_generation: grant.binding_generation,
    };
    let git = git(&state)?;
    let head = tokio::time::timeout(
        Duration::from_secs(5),
        git.show_ref(&target, &grant.grant.task_ref),
    )
    .await
    .map_err(|_| unavailable())?
    .map_err(|_| unavailable())?;
    if push && head != grant.grant.lease {
        return Err(super::refusal(Refusal::StaleLease));
    }
    // The workspace grants no try refs. Hold the relevant approval, binding,
    // and attempt locks through the upstream request.
    let response = aq_delivery::git_grant::forward(
        pool,
        socket,
        (&grant.grant, None),
        (repository, &path),
        request,
    )
    .await
    .map_err(super::refusal)?;
    tx.commit().await.map_err(crate::error::db)?;
    Ok(response.map(axum::body::Body::new).into_response())
}

async fn bundle(
    state: &AppState,
    node: &crate::worker_api::AuthenticatedNode,
    attempt: Uuid,
    grant: WorkspaceGrant,
    offset: u64,
) -> Result<Response, AppError> {
    let pool = &state.store.pool;
    let repository = grant.grant.repository_id;
    let generation = grant.binding_generation;
    let base = &grant.grant.base;
    let (size, digest, bytes) = crate::source_retention::profiled_base::workspace::chunk(
        state,
        grant.project_id,
        repository,
        generation,
        base,
        offset,
    )
    .await?;
    let mut tx = pool.begin().await.map_err(crate::error::db)?;
    crate::worker_api::read_current_node(&mut tx, node).await?;
    let current = live_grant_locked(&mut tx, attempt, node.id(), grant.project_id)
        .await
        .map_err(crate::error::db)?
        .ok_or_else(unauthorized)?;
    if current.project_revision != grant.project_revision
        || current.binding_generation != generation
        || current.access != grant.access
        || current.grant != grant.grant
        || current.checkout_head != grant.checkout_head
    {
        return Err(stale());
    }
    tx.commit().await.map_err(crate::error::db)?;
    Ok((
        [
            ("content-type", "application/x-git-bundle".into()),
            ("cache-control", "no-store".into()),
            ("aq-workspace-bundle-size", size.to_string()),
            ("aq-workspace-bundle-sha256", digest),
        ],
        bytes,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::checkout;

    #[test]
    fn sibling_checkout_resumes_at_only_the_verified_predecessor_head() {
        let original = "a".repeat(40);
        let retained = "b".repeat(40);
        let expected = Some(Some(retained.clone()));
        assert_eq!(
            checkout(&original, None, None).unwrap(),
            (original.clone(), None)
        );
        assert_eq!(
            checkout(&original, expected.clone(), Some(retained.clone())).unwrap(),
            (retained.clone(), Some(retained.clone()))
        );
        assert!(checkout(&original, None, Some(retained)).is_err());
        assert!(checkout(&original, expected.clone(), Some("c".repeat(40))).is_err());
        assert!(checkout(&original, expected, None).is_err());
        assert!(checkout(&original, Some(None), Some(original.clone())).is_err());
    }
}
