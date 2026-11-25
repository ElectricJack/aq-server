use crate::{
    AppState,
    authz::RequireOwner,
    error::{AppError, db},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, types::Json as SqlJson};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Definition {
    NotTracked,
    ConfirmedCompletion,
    TargetBranchMerge {
        repository_id: Uuid,
        target_ref: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    work_class: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    operation_id: Uuid,
    expected_revision: i64,
    work_class: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    definition: Option<Definition>,
}

#[derive(Serialize)]
struct Setting {
    project_id: Uuid,
    work_class: Option<String>,
    revision: i64,
    effective_at_ms: Option<i64>,
    definition: Option<Definition>,
    effective_definition: Definition,
}

type Revision = (i64, i64, Option<SqlJson<Definition>>);

#[derive(sqlx::FromRow)]
struct Replay {
    actor_id: Uuid,
    project_id: Uuid,
    work_class: String,
    revision: i64,
    effective_at_ms: i64,
    definition: Option<SqlJson<Definition>>,
}

const CURRENT: &str = "SELECT revision,effective_at_ms,definition FROM performance_delivery_definitions \
    WHERE project_id=$1 AND work_class=$2 ORDER BY revision DESC LIMIT 1";

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/settings/performance/delivery/{id}",
            get(read).post(write),
        )
        .layer(DefaultBodyLimit::max(4096))
}

fn bad(message: &str) -> AppError {
    AppError::BadRequest(message.into())
}

fn class(value: Option<&str>) -> Result<&str, AppError> {
    match value {
        None => Ok(""),
        Some(value)
            if !value.trim().is_empty()
                && value.trim() == value
                && value.len() <= 128
                && !value.chars().any(char::is_control) =>
        {
            Ok(value)
        }
        Some(_) => Err(bad("invalid work class")),
    }
}

fn valid_target(target: &str) -> bool {
    let Some(branch) = target.strip_prefix("refs/heads/") else {
        return false;
    };
    target.len() <= 255
        && !branch.ends_with('.')
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch
            .bytes()
            .any(|c| c.is_ascii_control() || b" ~^:?*[\\".contains(&c))
        && branch
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

async fn setting(
    db: &mut PgConnection,
    project: Uuid,
    work_class: &str,
    current: Option<Revision>,
    as_of: i64,
) -> Result<Setting, AppError> {
    let (revision, time, definition) = match current {
        Some((revision, time, definition)) => (revision, Some(time), definition.map(|v| v.0)),
        None => (0, None, None),
    };
    let inherited: Option<SqlJson<Definition>> = if definition.is_none() && !work_class.is_empty() {
        sqlx::query_scalar("SELECT definition FROM performance_delivery_definitions \
            WHERE project_id=$1 AND work_class='' AND effective_at_ms <= $2 ORDER BY revision DESC LIMIT 1")
            .bind(project).bind(as_of).fetch_optional(db).await.map_err(crate::error::db)?.flatten()
    } else {
        None
    };
    Ok(Setting {
        project_id: project,
        work_class: (!work_class.is_empty()).then(|| work_class.to_owned()),
        revision,
        effective_at_ms: time,
        effective_definition: definition
            .clone()
            .or_else(|| inherited.map(|v| v.0))
            .unwrap_or(Definition::NotTracked),
        definition,
    })
}

async fn project_repository(
    conn: &mut PgConnection,
    project: Uuid,
    lock: bool,
) -> Result<String, AppError> {
    let query = if lock {
        "SELECT binding->>'repository_id' FROM project_execution_bindings WHERE project_id=$1 FOR UPDATE"
    } else {
        "SELECT binding->>'repository_id' FROM project_execution_bindings WHERE project_id=$1"
    };
    sqlx::query_scalar(query)
        .bind(project)
        .fetch_optional(conn)
        .await
        .map_err(db)?
        .ok_or_else(|| AppError::NotFound("project not found".into()))
}

async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(project): Path<Uuid>,
    Query(scope): Query<Scope>,
) -> Result<Json<Setting>, AppError> {
    let work_class = class(scope.work_class.as_deref())?;
    let mut conn = state.store.pool.acquire().await.map_err(db)?;
    project_repository(&mut conn, project, false).await?;
    let current = sqlx::query_as(CURRENT)
        .bind(project)
        .bind(work_class)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db)?;
    Ok(Json(
        setting(&mut conn, project, work_class, current, i64::MAX).await?,
    ))
}

async fn write(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Path(project): Path<Uuid>,
    Json(change): Json<Change>,
) -> Result<Json<Setting>, AppError> {
    let work_class = class(change.work_class.as_deref())?;
    if change.operation_id.is_nil() || change.expected_revision < 0 {
        return Err(bad("invalid operation ID or expected revision"));
    }
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::uuid::text, 8172001))")
        .bind(change.operation_id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let repo = project_repository(&mut tx, project, true).await?;
    let replay: Option<Replay> = sqlx::query_as(
        "SELECT actor_id,project_id,work_class,revision,effective_at_ms,definition \
            FROM performance_delivery_definitions WHERE operation_id=$1",
    )
    .bind(change.operation_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    if let Some(saved) = replay {
        if saved.actor_id != actor.id
            || saved.project_id != project
            || saved.work_class != work_class
            || saved.revision - 1 != change.expected_revision
            || saved.definition.as_ref().map(|v| &v.0) != change.definition.as_ref()
        {
            return Err(AppError::Conflict("operation ID already used".into()));
        }
        let revision = Some((saved.revision, saved.effective_at_ms, saved.definition));
        return Ok(Json(
            setting(
                &mut tx,
                project,
                work_class,
                revision,
                saved.effective_at_ms,
            )
            .await?,
        ));
    }
    if let Some(Definition::TargetBranchMerge {
        repository_id,
        target_ref,
    }) = &change.definition
        && (Some(*repository_id) != Uuid::parse_str(&repo).ok() || !valid_target(target_ref))
    {
        return Err(bad(
            "use the project's repository and a full target branch ref",
        ));
    }
    let current: Option<Revision> = sqlx::query_as(CURRENT)
        .bind(project)
        .bind(work_class)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?;
    let revision = current.as_ref().map_or(0, |v| v.0);
    if revision != change.expected_revision {
        return Err(AppError::Conflict(
            "delivery settings revision changed".into(),
        ));
    }
    let latest: Option<i64> = sqlx::query_scalar(
        "SELECT max(effective_at_ms) FROM performance_delivery_definitions WHERE project_id=$1",
    )
    .bind(project)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    let now = (state.clock)().timestamp_millis();
    let time = now.max(latest.unwrap_or(0).saturating_add(1));
    let revision = revision.checked_add(1).ok_or(AppError::Internal)?;
    let definition = change.definition.map(SqlJson);
    sqlx::query("INSERT INTO performance_delivery_definitions \
        (project_id,work_class,revision,effective_at_ms,definition,operation_id,actor_id) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(project).bind(work_class).bind(revision).bind(time).bind(&definition)
        .bind(change.operation_id).bind(actor.id).execute(&mut *tx).await.map_err(db)?;
    let response = setting(
        &mut tx,
        project,
        work_class,
        Some((revision, time, definition)),
        time,
    )
    .await?;
    tx.commit().await.map_err(db)?;
    Ok(Json(response))
}
