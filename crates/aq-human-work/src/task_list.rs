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

pub struct ReadAt<'a> {
    pub pool: &'a PgPool,
    pub cursors: &'a CursorCodec,
    pub now_ms: i64,
}

type PageKey = (DateTime<Utc>, Uuid);
#[derive(Clone, Copy)]
struct PageWindow {
    limit: u16,
    before: Option<PageKey>,
}

impl Query {
    fn limit(&self) -> Result<u16, Error> {
        let limit = self.limit.unwrap_or(50);
        let valid_state = self
            .state
            .as_deref()
            .is_none_or(|s| super::STATES[..4].contains(&s));
        let valid_query = self.q.as_deref().is_none_or(|q| {
            !q.trim().is_empty() && q.chars().count() <= 256 && !q.chars().any(char::is_control)
        });
        ((1..=64).contains(&limit) && valid_state && valid_query)
            .then_some(limit)
            .ok_or(Error::Invalid)
    }

    async fn fetch(
        &self,
        workspaces: &BTreeSet<Uuid>,
        window: PageWindow,
        graph_read: impl AsyncFnOnce(Value) -> Result<Value, Error>,
    ) -> Result<GraphPage, Error> {
        if workspaces.is_empty() {
            return Ok(GraphPage {
                items: vec![],
                unavailable_workspaces: BTreeSet::new(),
                has_more: false,
            });
        }
        let raw = graph_read(json!({
            "workspace_ids":workspaces,"limit":window.limit,"q":self.q,"state":self.state,"before":window.before,
        })).await?;
        serde_json::from_value(raw).map_err(|_| Error::Unavailable)
    }
}

impl Owner<'_> {
    fn binding(
        &self,
        scope: &[ProjectBinding],
        query: &Query,
        limit: u16,
    ) -> Result<Binding, Error> {
        Ok(Binding {
            principal: self.id,
            authorization_partition: digest(&(&self.email, &self.issuer, &self.subject, scope))?,
            scope: json!({"kind":"all_authorized"}),
            filters: json!({"q":query.q,"state":query.state}),
            sort: "updated_at desc,id desc".into(),
            page_size: limit,
            resource: "work-task-list".into(),
        })
    }
}

pub async fn read(
    at: ReadAt<'_>,
    owner: Owner<'_>,
    query: Query,
    graph_read: impl AsyncFnOnce(Value) -> Result<Value, Error>,
) -> Result<Reply, Error> {
    let limit = query.limit()?;
    let now = u64::try_from(at.now_ms).map_err(|_| Error::Unavailable)?;
    let scope = projects(at.pool, &owner).await?;
    let binding = owner.binding(&scope, &query, limit)?;
    let position = query
        .cursor
        .as_deref()
        .map(|token| {
            at.cursors
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
    let window = PageWindow { limit, before };
    let page = query.fetch(&workspaces, window, graph_read).await?;
    let visible = page.validate(&workspaces, window, query.state.as_deref())?;
    let graph_scope = digest(&page.unavailable_workspaces)?;
    if position.is_some_and(|p| p.snapshot != graph_scope)
        || projects(at.pool, &owner).await? != scope
    {
        return Err(Error::Changed);
    }
    let next_cursor = page
        .last_key()?
        .map(|key| {
            at.cursors
                .encode(binding.position(&graph_scope, &key, now..now + CURSOR_TTL_MS))
                .map_err(|_| Error::Unavailable)
        })
        .transpose()?;
    Ok(page.reply(scope, visible, next_cursor))
}

impl GraphPage {
    fn last_key(&self) -> Result<Option<String>, Error> {
        if !self.has_more {
            return Ok(None);
        }
        let last = self.items.last().ok_or(Error::Unavailable)?;
        serde_json::to_string(&(last.updated_at, last.id))
            .map(Some)
            .map_err(|_| Error::Unavailable)
    }
    fn reply(
        self,
        scope: Vec<ProjectBinding>,
        visible: HashSet<Uuid>,
        next_cursor: Option<String>,
    ) -> Reply {
        let unavailable_projects = scope
            .iter()
            .filter(|p| {
                !p.graph_configured() || self.unavailable_workspaces.contains(&p.workspace_id)
            })
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
        Reply {
            contract: "human-work-task-list-1",
            items: self.items,
            projects,
            unavailable_projects,
            next_cursor,
        }
    }

    fn visible_workspace(&self, workspaces: &BTreeSet<Uuid>, id: Uuid) -> bool {
        workspaces.contains(&id) && !self.unavailable_workspaces.contains(&id)
    }

    fn within_limit(&self, limit: u16) -> bool {
        let len = self.items.len();
        len <= usize::from(limit) && (!self.has_more || len == usize::from(limit))
    }
    fn validate(
        &self,
        workspaces: &BTreeSet<Uuid>,
        window: PageWindow,
        state: Option<&str>,
    ) -> Result<HashSet<Uuid>, Error> {
        if !self.within_limit(window.limit) || !self.unavailable_workspaces.is_subset(workspaces) {
            return Err(Error::Unavailable);
        }
        let mut last = window.before;
        let mut ids = HashSet::new();
        let mut visible = HashSet::new();
        for item in &self.items {
            let key = (item.updated_at, item.id);
            let sorted = last.is_none_or(|previous| key < previous);
            let visible_workspace = self.visible_workspace(workspaces, item.workspace_id);
            let new_item = sorted && ids.insert(item.id);
            let valid_item = visible_workspace && new_item && item.matches(state);
            if !valid_item {
                return Err(Error::Unavailable);
            }
            visible.insert(item.workspace_id);
            last = Some(key);
        }
        Ok(visible)
    }
}

impl Task {
    fn matches(&self, state: Option<&str>) -> bool {
        let title = !self.title.trim().is_empty() && self.title.len() <= 512;
        let valid_state = super::STATES[..4].contains(&self.task_state.as_str())
            && state.is_none_or(|s| self.task_state == s);
        !self.id.is_nil() && title && valid_state
    }
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
    if !actor.matches(&user) {
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

impl Owner<'_> {
    fn matches(&self, user: &sqlx::postgres::PgRow) -> bool {
        let identity = user.get::<Option<&str>, _>("issuer") == self.issuer
            && user.get::<Option<&str>, _>("subject") == self.subject;
        identity
            && user.get::<String, _>("role") == "owner"
            && user.get::<String, _>("email") == self.email
    }
}
