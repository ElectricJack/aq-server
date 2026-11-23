//! A lightweight Work page over the current AQ-to-Graph project bindings.
use aq_human_cursor::{Binding, CursorCodec};
use aq_project_policy::ProjectRecord;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::collections::{BTreeSet, HashSet};
use uuid::Uuid;

const MAX_PROJECTS: usize = 4096;
const CURSOR_TTL_MS: u64 = 15 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Work list parameters")]
    Invalid,
    #[error("Work list scope or cursor changed")]
    Changed,
    #[error("Work list is unavailable")]
    Unavailable,
}

pub struct Owner<'a> {
    pub id: Uuid,
    pub email: &'a str,
    pub issuer: Option<&'a str>,
    pub subject: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    limit: Option<u16>,
    cursor: Option<String>,
    q: Option<String>,
    state: Option<String>,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct ProjectBinding {
    id: Uuid,
    workspace_id: Uuid,
    name: String,
    revision: i64,
    graph_adapter: Option<String>,
}

impl ProjectBinding {
    fn graph_configured(&self) -> bool {
        !self.workspace_id.is_nil() && self.graph_adapter.as_deref() == Some("aq.graph.configured")
    }
}

#[derive(Serialize)]
struct Project {
    id: Uuid,
    workspace_id: Uuid,
    name: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    id: Uuid,
    workspace_id: Uuid,
    title: String,
    task_state: String,
    updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphPage {
    items: Vec<Task>,
    unavailable_workspaces: BTreeSet<Uuid>,
    has_more: bool,
}

#[derive(Serialize)]
pub struct Reply {
    contract: &'static str,
    items: Vec<Task>,
    projects: Vec<Project>,
    unavailable_projects: usize,
    next_cursor: Option<String>,
}

async fn projects(pool: &PgPool, owner: &Owner<'_>) -> Result<Vec<ProjectBinding>, Error> {
    // A single statement checks the current owner and reads only the metadata needed for Work.
    let raw: Option<Value> = sqlx::query_scalar(
        "SELECT COALESCE((SELECT jsonb_agg(p ORDER BY p.id) FROM (
            SELECT project_id AS id, binding->>'graph_workspace_id' AS workspace_id,
                   binding->>'name' AS name, revision, binding->>'graph_adapter_reference' AS graph_adapter
            FROM project_execution_bindings ORDER BY project_id LIMIT $5
         ) p), '[]') FROM users
         WHERE id=$1 AND role='owner' AND email=$2
           AND issuer IS NOT DISTINCT FROM $3 AND subject IS NOT DISTINCT FROM $4",
    )
    .bind(owner.id)
    .bind(owner.email)
    .bind(owner.issuer)
    .bind(owner.subject)
    .bind((MAX_PROJECTS + 1) as i64)
    .fetch_optional(pool)
    .await
    .map_err(|_| Error::Unavailable)?;
    let projects: Vec<ProjectBinding> =
        serde_json::from_value(raw.ok_or(Error::Changed)?).map_err(|_| Error::Unavailable)?;
    if projects.len() > MAX_PROJECTS {
        return Err(Error::Unavailable);
    }
    Ok(projects)
}

fn digest(value: &impl Serialize) -> Result<String, Error> {
    let bytes = serde_json::to_vec(value).map_err(|_| Error::Unavailable)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub async fn read(
    pool: &PgPool,
    cursors: &CursorCodec,
    owner: Owner<'_>,
    query: Query,
    now_ms: i64,
    graph_read: impl AsyncFnOnce(Value) -> Result<Value, Error>,
) -> Result<Reply, Error> {
    let limit = query.limit.unwrap_or(50);
    if !(1..=64).contains(&limit)
        || query
            .state
            .as_ref()
            .is_some_and(|s| !super::STATES[..4].contains(&s.as_str()))
        || query.q.as_ref().is_some_and(|q| {
            q.trim().is_empty() || q.chars().count() > 256 || q.chars().any(char::is_control)
        })
    {
        return Err(Error::Invalid);
    }
    let now = u64::try_from(now_ms).map_err(|_| Error::Unavailable)?;
    let scope = projects(pool, &owner).await?;
    let binding = Binding {
        principal: owner.id,
        authorization_partition: digest(&(&owner.email, &owner.issuer, &owner.subject, &scope))?,
        scope: json!({"kind":"all_authorized"}),
        filters: json!({"q":query.q,"state":query.state}),
        sort: "updated_at desc,id desc".into(),
        page_size: limit,
        resource: "work-task-list".into(),
    };
    let position = query
        .cursor
        .as_deref()
        .map(|token| {
            cursors
                .decode(token, &binding, now)
                .map_err(|_| Error::Changed)
        })
        .transpose()?;
    let before: Option<(DateTime<Utc>, Uuid)> = position
        .as_ref()
        .map(|p| serde_json::from_str(&p.position).map_err(|_| Error::Changed))
        .transpose()?;
    let workspaces: BTreeSet<_> = scope
        .iter()
        .filter(|p| p.graph_configured())
        .map(|p| p.workspace_id)
        .collect();
    let page = if workspaces.is_empty() {
        GraphPage {
            items: vec![],
            unavailable_workspaces: BTreeSet::new(),
            has_more: false,
        }
    } else {
        let raw = graph_read(json!({
            "workspace_ids":workspaces,"limit":limit,"q":query.q,"state":query.state,"before":before,
        })).await?;
        serde_json::from_value::<GraphPage>(raw).map_err(|_| Error::Unavailable)?
    };
    let visible = validate(&page, &workspaces, limit, before, query.state.as_deref())?;
    let graph_scope = digest(&page.unavailable_workspaces)?;
    if position.is_some_and(|p| p.snapshot != graph_scope) || projects(pool, &owner).await? != scope
    {
        return Err(Error::Changed);
    }
    let next_cursor = if page.has_more {
        let last = page.items.last().ok_or(Error::Unavailable)?;
        let key =
            serde_json::to_string(&(last.updated_at, last.id)).map_err(|_| Error::Unavailable)?;
        Some(
            cursors
                .encode(binding.position(&graph_scope, &key, now..now + CURSOR_TTL_MS))
                .map_err(|_| Error::Unavailable)?,
        )
    } else {
        None
    };
    let unavailable_projects = scope
        .iter()
        .filter(|p| !p.graph_configured() || page.unavailable_workspaces.contains(&p.workspace_id))
        .count();
    let projects = scope
        .into_iter()
        .filter(|p| p.graph_configured() && visible.contains(&p.workspace_id))
        .map(|p| Project {
            id: p.id,
            workspace_id: p.workspace_id,
            name: p.name,
        })
        .collect();
    Ok(Reply {
        contract: "human-work-task-list-1",
        items: page.items,
        projects,
        unavailable_projects,
        next_cursor,
    })
}

fn validate(
    page: &GraphPage,
    workspaces: &BTreeSet<Uuid>,
    limit: u16,
    before: Option<(DateTime<Utc>, Uuid)>,
    state: Option<&str>,
) -> Result<HashSet<Uuid>, Error> {
    if page.items.len() > usize::from(limit)
        || (page.has_more && page.items.len() != usize::from(limit))
        || !page.unavailable_workspaces.is_subset(workspaces)
    {
        return Err(Error::Unavailable);
    }
    let mut last = before;
    let mut ids = HashSet::new();
    let mut visible = HashSet::new();
    for item in &page.items {
        let key = (item.updated_at, item.id);
        if item.id.is_nil()
            || !ids.insert(item.id)
            || last.is_some_and(|previous| key >= previous)
            || !workspaces.contains(&item.workspace_id)
            || page.unavailable_workspaces.contains(&item.workspace_id)
            || item.title.trim().is_empty()
            || item.title.len() > 512
            || !super::STATES[..4].contains(&item.task_state.as_str())
            || state.is_some_and(|state| item.task_state != state)
        {
            return Err(Error::Unavailable);
        }
        visible.insert(item.workspace_id);
        last = Some(key);
    }
    Ok(visible)
}

/// Final current-role and binding fence, with one bounded project batch query.
pub async fn fence_records(
    pool: &PgPool,
    actor: &Owner<'_>,
    records: &[ProjectRecord],
) -> Result<(), Error> {
    if records.len() > 8 {
        return Err(Error::Invalid);
    }
    let mut tx = pool.begin().await.map_err(|_| Error::Unavailable)?;
    sqlx::query("SET LOCAL statement_timeout='1500ms'")
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::Unavailable)?;
    sqlx::query("SET LOCAL lock_timeout='500ms'")
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::Unavailable)?;
    let user = sqlx::query("SELECT role,email,issuer,subject FROM users WHERE id=$1 FOR SHARE")
        .bind(actor.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| Error::Unavailable)?
        .ok_or(Error::Changed)?;
    if user.get::<String, _>("role") != "owner"
        || user.get::<String, _>("email") != actor.email
        || user.get::<Option<&str>, _>("issuer") != actor.issuer
        || user.get::<Option<&str>, _>("subject") != actor.subject
    {
        return Err(Error::Changed);
    }
    let ids: Vec<Uuid> = records
        .iter()
        .map(|record| record.binding.project_id)
        .collect();
    let rows=sqlx::query("SELECT project_id,revision,binding FROM project_execution_bindings WHERE project_id=ANY($1) ORDER BY project_id FOR SHARE").bind(&ids).fetch_all(&mut *tx).await.map_err(|_|Error::Unavailable)?;
    if rows.len() != records.len() {
        return Err(Error::Changed);
    }
    for latest in rows {
        let expected = records
            .iter()
            .find(|record| record.binding.project_id == latest.get::<Uuid, _>("project_id"))
            .ok_or(Error::Changed)?;
        if latest.get::<i64, _>("revision") as u64 != expected.revision
            || latest.get::<Value, _>("binding")
                != serde_json::to_value(&expected.binding).map_err(|_| Error::Unavailable)?
        {
            return Err(Error::Changed);
        }
    }
    tx.commit().await.map_err(|_| Error::Unavailable)?;
    Ok(())
}
