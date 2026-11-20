//! Task comment threads (spec aq-dashboard 0004 §4.1): the discussion on one
//! task, keyed by `(project_id, task_id)` in AQ's own database. The Graph
//! stays the record of task structure and content; the thread is operational.
//!
//! Callers own authentication and authorization (owner role, project binding,
//! the agent's per-attempt credential). This crate owns body validation, the
//! store, idempotent operation replay, ownership and revision checks, paging
//! and the wire view shared by the human and agent routes.
//!
//! - Every mutation carries an operation ID. Replaying the same operation with
//!   the same request returns the recorded effect; a changed request under the
//!   same ID is [`Error::IdConflict`]. An added comment's ID is its operation ID.
//! - Edit and delete target an exact `expected_revision`; a stale one is
//!   [`Error::Conflict`] with the current revision and changes nothing. Only
//!   the author may edit or delete ([`Error::NotAuthor`]).
//! - Delete is a tombstone: the body is cleared, the row and its position stay.
//! - Threads list newest first (insertion order), at most [`MAX_LIMIT`] a page.
use chrono::{DateTime, SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, postgres::PgRow};
use uuid::Uuid;

/// Markdown body limit in bytes.
pub const MAX_BODY_BYTES: usize = 16 * 1024;
pub const DEFAULT_LIMIT: u16 = 50;
pub const MAX_LIMIT: u16 = 100;
/// Entity type of a comment in action subjects, receipts and versions.
pub const ENTITY: &str = "task_comment";
pub const ADD: &str = "task.comment.add";
pub const EDIT: &str = "task.comment.edit";
pub const DELETE: &str = "task.comment.delete";
/// Agent routes; published in `protocol/agent-comments/v1/routes.json`.
pub const AGENT_ROUTES: [(&str, &str); 4] = [
    ("/v1/agent/comments", "comment.add"),
    ("/v1/agent/comments/list", "comment.list"),
    ("/v1/agent/comments/edit", "comment.edit"),
    ("/v1/agent/comments/delete", "comment.delete"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Malformed body, revision, limit or cursor.
    Invalid,
    /// No such comment in this project.
    NotFound,
    /// The actor did not write this comment.
    NotAuthor,
    /// The comment is already deleted.
    Deleted,
    /// `expected_revision` is stale; carries the current revision.
    Conflict(i64),
    /// The operation ID was used for a different request.
    IdConflict,
    Unavailable,
}
impl From<sqlx::Error> for Error {
    fn from(_: sqlx::Error) -> Self {
        Self::Unavailable
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::Unavailable
    }
}
impl Error {
    /// The agent-access error code for this error.
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid | Self::Deleted => "invalid_request",
            Self::NotFound => "not_found",
            Self::NotAuthor => "scope_denied",
            Self::Conflict(_) => "stale_revision",
            Self::IdConflict => "id_conflict",
            Self::Unavailable => "internal",
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;

/// Who wrote a comment. Stored as `author_kind` plus this value as `author_ref`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Author {
    Owner { user_id: Uuid },
    Agent { session_id: Uuid, attempt_id: Uuid },
    Supervisor { session_id: Uuid },
    System { component: String },
}
impl Author {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Owner { .. } => "owner",
            Self::Agent { .. } => "agent",
            Self::Supervisor { .. } => "supervisor",
            Self::System { .. } => "system",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thread {
    pub project_id: Uuid,
    pub task_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub comment_id: Uuid,
    pub project_id: Uuid,
    pub task_id: Uuid,
    pub author: Author,
    /// The owner's current email for owner authors.
    pub author_display: Option<String>,
    /// `None` once deleted.
    pub body: Option<String>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub revision: i64,
    pub seq: i64,
}
fn time(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}
impl Comment {
    /// Wire view. `can_edit` is true only for the viewing owner's own live comment.
    pub fn view(&self, viewer: Option<Uuid>) -> Value {
        let (user_id, session_id, attempt_id, display) = match &self.author {
            Author::Owner { user_id } => (Some(*user_id), None, None, self.author_display.clone()),
            Author::Agent {
                session_id,
                attempt_id,
            } => (None, Some(*session_id), Some(*attempt_id), None),
            Author::Supervisor { session_id } => (None, Some(*session_id), None, None),
            Author::System { component } => (None, None, None, Some(component.clone())),
        };
        let mine = viewer.is_some_and(|v| self.author == Author::Owner { user_id: v });
        json!({
            "comment_id": self.comment_id,
            "task_id": self.task_id,
            "author": {"kind": self.author.kind(), "user_id": user_id, "display_name": display,
                "session_id": session_id, "attempt_id": attempt_id},
            "body": self.body,
            "created_at": time(&self.created_at),
            "edited_at": self.edited_at.as_ref().map(time),
            "deleted_at": self.deleted_at.as_ref().map(time),
            "revision": self.revision.to_string(),
            "can_edit": mine && self.deleted_at.is_none(),
        })
    }
    fn from_row(row: &PgRow) -> Result<Self> {
        let author: Author = serde_json::from_value(row.try_get("author_ref")?)?;
        if author.kind() != row.try_get::<String, _>("author_kind")? {
            return Err(Error::Unavailable);
        }
        Ok(Self {
            comment_id: row.try_get("comment_id")?,
            project_id: row.try_get("project_id")?,
            task_id: row.try_get("task_id")?,
            author,
            author_display: row.try_get("author_display")?,
            body: row.try_get("body")?,
            created_at: row.try_get("created_at")?,
            edited_at: row.try_get("edited_at")?,
            deleted_at: row.try_get("deleted_at")?,
            revision: row.try_get("revision")?,
            seq: row.try_get("seq")?,
        })
    }
}

/// Nonblank Markdown of at most [`MAX_BODY_BYTES`]; no control characters
/// other than tab, line feed and carriage return.
pub fn validate_body(body: &str) -> Result<()> {
    if body.trim().is_empty()
        || body.len() > MAX_BODY_BYTES
        || body
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
/// A canonical positive decimal revision.
pub fn revision(value: &str) -> Option<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|r| *r > 0 && r.to_string() == value)
}
/// Page size: default [`DEFAULT_LIMIT`], 1..=[`MAX_LIMIT`].
pub fn limit(value: Option<u16>) -> Result<u16> {
    match value.unwrap_or(DEFAULT_LIMIT) {
        n @ 1..=MAX_LIMIT => Ok(n),
        _ => Err(Error::Invalid),
    }
}

/// One dashboard action payload (`dashboard-actions-1`).
pub trait Command: Clone + DeserializeOwned + JsonSchema + Send + Sync + 'static {
    const KIND: &'static str;
    fn operation_id(&self) -> Uuid;
    fn project_id(&self) -> Uuid;
    fn expected_revision(&self) -> Option<&str> {
        None
    }
    fn body(&self) -> Option<&str> {
        None
    }
}
/// `task.comment.add`: the subject is `{type:"task",id}` with no revision.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddComment {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    /// Markdown, at most 16 KiB.
    #[schemars(length(min = 1, max = 16384))]
    pub body: String,
}
/// `task.comment.edit`: the subject is `{type:"task_comment",id,revision}`.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditComment {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_revision: String,
    #[schemars(length(min = 1, max = 16384))]
    pub body: String,
}
/// `task.comment.delete`: the subject is `{type:"task_comment",id,revision}`.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteComment {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_revision: String,
}
macro_rules! command {
    ($t:ty, $kind:expr) => {
        impl Command for $t {
            const KIND: &'static str = $kind;
            fn operation_id(&self) -> Uuid {
                self.operation_id
            }
            fn project_id(&self) -> Uuid {
                self.project_id
            }
            fn expected_revision(&self) -> Option<&str> {
                self.expected()
            }
            fn body(&self) -> Option<&str> {
                self.text()
            }
        }
    };
}
impl AddComment {
    fn expected(&self) -> Option<&str> {
        None
    }
    fn text(&self) -> Option<&str> {
        Some(&self.body)
    }
}
impl EditComment {
    fn expected(&self) -> Option<&str> {
        Some(&self.expected_revision)
    }
    fn text(&self) -> Option<&str> {
        Some(&self.body)
    }
}
impl DeleteComment {
    fn expected(&self) -> Option<&str> {
        Some(&self.expected_revision)
    }
    fn text(&self) -> Option<&str> {
        None
    }
}
command!(AddComment, ADD);
command!(EditComment, EDIT);
command!(DeleteComment, DELETE);

/// A committed mutation: the comment now, and the revision this operation made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub comment: Comment,
    pub revision: i64,
}
/// The recorded effect of one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub kind: String,
    pub comment_id: Uuid,
    pub project_id: Uuid,
    pub task_id: Uuid,
    pub actor: Author,
    pub revision: i64,
}

const SELECT: &str = "SELECT c.*, u.email AS author_display FROM task_comments c \
    LEFT JOIN users u ON c.author_kind='owner' AND u.id=(c.author_ref->>'user_id')::uuid";

pub async fn get(pool: &PgPool, comment_id: Uuid) -> Result<Option<Comment>> {
    read(pool, comment_id).await
}

async fn read(db: impl sqlx::PgExecutor<'_>, comment_id: Uuid) -> Result<Option<Comment>> {
    sqlx::query(&format!("{SELECT} WHERE c.comment_id=$1"))
        .bind(comment_id)
        .fetch_optional(db)
        .await?
        .as_ref()
        .map(Comment::from_row)
        .transpose()
}
/// The task of `comment_id` in `project_id` when `author` wrote it (the
/// scope target of an agent's edit or delete).
pub async fn authored_task(
    pool: &PgPool,
    comment_id: Uuid,
    project_id: Uuid,
    author: &Author,
) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT task_id FROM task_comments WHERE comment_id=$1 AND project_id=$2 AND author_ref=$3",
    )
    .bind(comment_id)
    .bind(project_id)
    .bind(serde_json::to_value(author)?)
    .fetch_optional(pool)
    .await?)
}
pub async fn operation(pool: &PgPool, operation_id: Uuid) -> Result<Option<Recorded>> {
    let row = sqlx::query("SELECT o.kind,o.comment_id,o.actor,o.revision,c.project_id,c.task_id FROM task_comment_operations o JOIN task_comments c USING(comment_id) WHERE o.operation_id=$1")
        .bind(operation_id)
        .fetch_optional(pool)
        .await?;
    row.map(|r| {
        Ok(Recorded {
            kind: r.try_get("kind")?,
            comment_id: r.try_get("comment_id")?,
            project_id: r.try_get("project_id")?,
            task_id: r.try_get("task_id")?,
            actor: serde_json::from_value(r.try_get("actor")?)?,
            revision: r.try_get("revision")?,
        })
    })
    .transpose()
}

/// One page, newest first, strictly older than `before` (a position from a
/// previous page). Returns the position of the next page when there is one.
pub async fn list(
    pool: &PgPool,
    thread: Thread,
    before: Option<i64>,
    limit: u16,
) -> Result<(Vec<Comment>, Option<i64>)> {
    let limit = self::limit(Some(limit))?;
    let rows = sqlx::query(&format!(
        "{SELECT} WHERE c.project_id=$1 AND c.task_id=$2 AND ($3::bigint IS NULL OR c.seq<$3) ORDER BY c.seq DESC LIMIT $4"
    ))
    .bind(thread.project_id)
    .bind(thread.task_id)
    .bind(before)
    .bind(i64::from(limit) + 1)
    .fetch_all(pool)
    .await?;
    let mut items = rows
        .iter()
        .map(Comment::from_row)
        .collect::<Result<Vec<_>>>()?;
    let more = items.len() > usize::from(limit);
    items.truncate(usize::from(limit));
    let next = more.then(|| items.last().map(|c| c.seq)).flatten();
    Ok((items, next))
}

/// Apply one action. `target` is the task for `task.comment.add` and the
/// comment for edit and delete; the comment must be in `cmd.project_id()`.
pub async fn apply<C: Command>(
    pool: &PgPool,
    cmd: &C,
    target: Uuid,
    actor: &Author,
    now: DateTime<Utc>,
) -> Result<Applied> {
    let expected = match cmd.expected_revision() {
        Some(v) => Some(revision(v).ok_or(Error::Invalid)?),
        None => None,
    };
    mutate(
        pool,
        C::KIND,
        cmd.operation_id(),
        cmd.project_id(),
        target,
        actor,
        expected,
        cmd.body(),
        now,
    )
    .await
}
/// Add a comment to `thread` (the agent route). The comment ID is the operation ID.
pub async fn add(
    pool: &PgPool,
    operation_id: Uuid,
    thread: Thread,
    author: &Author,
    body: &str,
    now: DateTime<Utc>,
) -> Result<Applied> {
    mutate(
        pool,
        ADD,
        operation_id,
        thread.project_id,
        thread.task_id,
        author,
        None,
        Some(body),
        now,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn mutate(
    pool: &PgPool,
    kind: &str,
    operation_id: Uuid,
    project_id: Uuid,
    target: Uuid,
    actor: &Author,
    expected: Option<i64>,
    body: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Applied> {
    let mut tx = pool.begin().await?;
    let applied = mutate_in_tx(
        &mut tx,
        kind,
        operation_id,
        project_id,
        target,
        actor,
        expected,
        body,
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(applied)
}

/// Add a comment as part of a caller's database transaction.
pub async fn add_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    operation_id: Uuid,
    thread: Thread,
    author: &Author,
    body: &str,
    now: DateTime<Utc>,
) -> Result<Applied> {
    mutate_in_tx(
        tx,
        ADD,
        operation_id,
        thread.project_id,
        thread.task_id,
        author,
        None,
        Some(body),
        now,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn mutate_in_tx(
    tx: &mut sqlx::PgConnection,
    kind: &str,
    operation_id: Uuid,
    project_id: Uuid,
    target: Uuid,
    actor: &Author,
    expected: Option<i64>,
    body: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Applied> {
    if [operation_id, project_id, target].iter().any(Uuid::is_nil) {
        return Err(Error::Invalid);
    }
    if let Some(body) = body {
        validate_body(body)?;
    }
    let add = kind == ADD;
    if add != expected.is_none() || (kind == DELETE) != body.is_none() {
        return Err(Error::Invalid);
    }
    // Postgres keeps microseconds; store what is read back.
    let now = DateTime::from_timestamp_micros(now.timestamp_micros()).ok_or(Error::Invalid)?;
    let comment_id = if add { operation_id } else { target };
    let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&json!([
        kind, project_id, target, expected, body
    ]))?));
    let actor_value = serde_json::to_value(actor)?;
    // The operation row serializes concurrent replays of one operation ID.
    let fresh = sqlx::query("INSERT INTO task_comment_operations(operation_id,kind,comment_id,actor,fingerprint,revision,created_at) VALUES($1,$2,$3,$4,$5,0,$6) ON CONFLICT DO NOTHING")
        .bind(operation_id)
        .bind(kind)
        .bind(comment_id)
        .bind(&actor_value)
        .bind(&fingerprint)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if !fresh {
        let row = sqlx::query("SELECT kind,actor,fingerprint,revision FROM task_comment_operations WHERE operation_id=$1")
            .bind(operation_id)
            .fetch_one(&mut *tx)
            .await?;
        if row.try_get::<String, _>("kind")? != kind
            || row.try_get::<Value, _>("actor")? != actor_value
            || row.try_get::<String, _>("fingerprint")? != fingerprint
        {
            return Err(Error::IdConflict);
        }
        let comment = read(&mut *tx, comment_id)
            .await?
            .ok_or(Error::Unavailable)?;
        return Ok(Applied {
            comment,
            revision: row.try_get("revision")?,
        });
    }
    let revision: i64 = if add {
        sqlx::query("INSERT INTO task_comments(comment_id,project_id,task_id,author_kind,author_ref,body,created_at,revision) VALUES($1,$2,$3,$4,$5,$6,$7,1)")
            .bind(comment_id)
            .bind(project_id)
            .bind(target)
            .bind(actor.kind())
            .bind(&actor_value)
            .bind(body)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(|e| match e {
                sqlx::Error::Database(d) if d.is_unique_violation() => Error::IdConflict,
                _ => Error::Unavailable,
            })?;
        1
    } else {
        let row = sqlx::query("SELECT project_id,author_ref,deleted_at,revision FROM task_comments WHERE comment_id=$1 FOR UPDATE")
            .bind(comment_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        if row.try_get::<Uuid, _>("project_id")? != project_id {
            return Err(Error::NotFound);
        }
        if row.try_get::<Value, _>("author_ref")? != actor_value {
            return Err(Error::NotAuthor);
        }
        if row
            .try_get::<Option<DateTime<Utc>>, _>("deleted_at")?
            .is_some()
        {
            return Err(Error::Deleted);
        }
        let current: i64 = row.try_get("revision")?;
        if Some(current) != expected {
            return Err(Error::Conflict(current));
        }
        let sql = if kind == DELETE {
            "UPDATE task_comments SET body=NULL,deleted_at=$2,revision=revision+1 WHERE comment_id=$1 RETURNING revision"
        } else {
            "UPDATE task_comments SET body=$3,edited_at=$2,revision=revision+1 WHERE comment_id=$1 RETURNING revision"
        };
        sqlx::query_scalar(sql)
            .bind(comment_id)
            .bind(now)
            .bind(body)
            .fetch_one(&mut *tx)
            .await?
    };
    sqlx::query("UPDATE task_comment_operations SET revision=$2 WHERE operation_id=$1")
        .bind(operation_id)
        .bind(revision)
        .execute(&mut *tx)
        .await?;
    let comment = read(&mut *tx, comment_id)
        .await?
        .ok_or(Error::Unavailable)?;
    Ok(Applied { comment, revision })
}

/// Opaque agent page cursor. The agent's task comes from its credential, so
/// the cursor is only a position and grants nothing.
pub fn agent_cursor(seq: i64) -> String {
    format!("tc1.{seq}")
}
fn agent_position(cursor: &str) -> Result<i64> {
    cursor
        .strip_prefix("tc1.")
        .and_then(|n| n.parse::<i64>().ok())
        .filter(|n| *n > 0 && cursor == agent_cursor(*n))
        .ok_or(Error::Invalid)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentAdd {
    api_version: u32,
    operation_id: Uuid,
    body: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentList {
    api_version: u32,
    /// The thread's task; the caller resolves and checks it ([`agent_list_task`]).
    #[serde(default, rename = "task_id")]
    _task_id: Option<Uuid>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<u16>,
}
/// `POST /v1/agent/comments` for an authenticated attempt on `thread`.
/// Errors are agent-access error codes.
pub async fn agent_add(
    pool: &PgPool,
    thread: Thread,
    author: &Author,
    body: &[u8],
    now: DateTime<Utc>,
) -> std::result::Result<Value, &'static str> {
    let b: AgentAdd = serde_json::from_slice(body).map_err(|_| "invalid_request")?;
    if b.api_version != 1 {
        return Err("unsupported_api_version");
    }
    let applied = add(pool, b.operation_id, thread, author, &b.body, now)
        .await
        .map_err(Error::code)?;
    Ok(json!({"api_version": 1, "comment": applied.comment.view(None)}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentChange {
    api_version: u32,
    operation_id: Uuid,
    comment_id: Uuid,
    expected_revision: String,
    #[serde(default)]
    body: Option<String>,
}
/// The task a comment list names (`task_id`), read before the grant check.
/// A body that does not name one lists the attempt's own task (and a
/// malformed one is refused once the grant is checked).
pub fn agent_list_task(body: &[u8]) -> Option<Uuid> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("task_id")?.as_str()?.parse().ok()
}
/// `POST /v1/agent/comments/edit` (`kind` [`EDIT`]) and `/delete` ([`DELETE`]):
/// a comment `author` wrote in `project_id`, at its expected revision. Another
/// author's comment is `scope_denied`, a stale revision `stale_revision`.
pub async fn agent_change(
    pool: &PgPool,
    kind: &str,
    project_id: Uuid,
    author: &Author,
    body: &[u8],
    now: DateTime<Utc>,
) -> std::result::Result<Value, &'static str> {
    let b: AgentChange = serde_json::from_slice(body).map_err(|_| "invalid_request")?;
    if b.api_version != 1 {
        return Err("unsupported_api_version");
    }
    let expected = revision(&b.expected_revision).ok_or("invalid_request")?;
    if ![EDIT, DELETE].contains(&kind) || (kind == DELETE) != b.body.is_none() {
        return Err("invalid_request");
    }
    let applied = mutate(
        pool,
        kind,
        b.operation_id,
        project_id,
        b.comment_id,
        author,
        Some(expected),
        b.body.as_deref(),
        now,
    )
    .await
    .map_err(Error::code)?;
    Ok(
        json!({"api_version": 1, "comment": applied.comment.view(None), "revision": applied.revision.to_string()}),
    )
}
/// `POST /v1/agent/comments/list`: a task thread (the attempt's own unless
/// `task_id` names another the caller may read), newest first.
pub async fn agent_list(
    pool: &PgPool,
    thread: Thread,
    body: &[u8],
) -> std::result::Result<Value, &'static str> {
    let b: AgentList = serde_json::from_slice(body).map_err(|_| "invalid_request")?;
    if b.api_version != 1 {
        return Err("unsupported_api_version");
    }
    let limit = limit(b.limit).map_err(Error::code)?;
    let before = b
        .cursor
        .as_deref()
        .map(agent_position)
        .transpose()
        .map_err(Error::code)?;
    let (items, next) = list(pool, thread, before, limit)
        .await
        .map_err(Error::code)?;
    let items: Vec<Value> = items.iter().map(|c| c.view(None)).collect();
    Ok(
        json!({"api_version": 1, "task_id": thread.task_id, "items": items, "next_cursor": next.map(agent_cursor)}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agent_comment_routes_match_contract() {
        let routes: Value = serde_json::from_str(include_str!(
            "../../../protocol/agent-comments/v1/routes.json"
        ))
        .unwrap();
        let listed = routes["routes"].as_array().unwrap();
        assert_eq!(listed.len(), AGENT_ROUTES.len());
        for (path, capability) in AGENT_ROUTES {
            let entry = listed.iter().find(|r| r["path"] == path).unwrap();
            assert_eq!(entry["capability"], capability);
            assert_eq!(entry["method"], "POST");
        }
        assert_eq!(routes["owner_actions"], json!([ADD, EDIT, DELETE]));
    }
    #[test]
    fn bodies_revisions_limits_and_cursors_are_bounded() {
        assert!(validate_body("Looks good.\n\n- [x] tests\tok\r\n").is_ok());
        for bad in ["", "  \n", "bell\u{7}", &"a".repeat(MAX_BODY_BYTES + 1)] {
            assert_eq!(validate_body(bad), Err(Error::Invalid));
        }
        assert!(validate_body(&"é".repeat(MAX_BODY_BYTES / 2)).is_ok());
        assert_eq!(revision("3"), Some(3));
        for bad in ["0", "03", "-1", "x", "+1"] {
            assert_eq!(revision(bad), None);
        }
        assert_eq!(limit(None), Ok(50));
        assert_eq!(limit(Some(100)), Ok(100));
        assert_eq!(limit(Some(0)), Err(Error::Invalid));
        assert_eq!(limit(Some(101)), Err(Error::Invalid));
        assert_eq!(agent_position(&agent_cursor(7)), Ok(7));
        for bad in ["tc1.0", "tc1.07", "7", "tc2.7", "tc1.-7"] {
            assert_eq!(agent_position(bad), Err(Error::Invalid));
        }
    }
    #[test]
    fn view_hides_deleted_bodies_and_only_the_author_owner_can_edit() {
        let owner = Uuid::from_u128(1);
        let at = DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
        let mut c = Comment {
            comment_id: Uuid::from_u128(2),
            project_id: Uuid::from_u128(3),
            task_id: Uuid::from_u128(4),
            author: Author::Owner { user_id: owner },
            author_display: Some("owner@localhost".into()),
            body: Some("hi".into()),
            created_at: at,
            edited_at: None,
            deleted_at: None,
            revision: 1,
            seq: 1,
        };
        let v = c.view(Some(owner));
        assert_eq!(v["can_edit"], true);
        assert_eq!(v["author"]["display_name"], "owner@localhost");
        assert_eq!(v["created_at"], "2023-11-14T22:13:20.000Z");
        assert_eq!(c.view(Some(Uuid::from_u128(9)))["can_edit"], false);
        assert_eq!(c.view(None)["can_edit"], false);
        c.body = None;
        c.deleted_at = Some(at);
        assert_eq!(c.view(Some(owner))["can_edit"], false);
        assert_eq!(c.view(Some(owner))["body"], Value::Null);
        let agent = Author::Agent {
            session_id: Uuid::from_u128(5),
            attempt_id: Uuid::from_u128(6),
        };
        assert_eq!(
            serde_json::to_value(&agent).unwrap(),
            json!({"kind":"agent","session_id":Uuid::from_u128(5),"attempt_id":Uuid::from_u128(6)})
        );
    }
}
