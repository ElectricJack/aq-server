//! Native provider messages are stored independently of the terminal archive.
use super::*;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::{get, post},
};

const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

pub(super) fn human_routes() -> Router<Service> {
    Router::new()
        .route("/api/manual-agents/{id}/conversation", get(read_manual))
        .route("/api/v1/sessions/{id}/conversation", get(read_attempt))
}

pub(super) fn worker_routes() -> Router<Service> {
    Router::new()
        .route(
            "/v1/worker/manual-agents/{id}/conversation",
            post(save_manual),
        )
        .route("/v1/worker/attempts/{id}/conversation", post(save_attempt))
        .layer(DefaultBodyLimit::max(MAX_SNAPSHOT_BYTES))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    messages: Vec<Message>,
    partial: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    User,
    Assistant,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Message {
    role: Role,
    text: String,
}

#[derive(Clone, Copy)]
enum Subject {
    Manual,
    Attempt,
}

impl Subject {
    fn read_query(self) -> &'static str {
        match self {
            Self::Manual => {
                "SELECT m.project_id,c.snapshot FROM manual_agent_sessions m
                LEFT JOIN manual_conversations c ON c.session_id=m.id WHERE m.id=$1"
            }
            Self::Attempt => {
                "SELECT (a.state#>>'{offer,manifest,bindings,project_id}')::uuid,c.snapshot
                FROM execution_attempts a LEFT JOIN attempt_conversations c USING(attempt_id)
                WHERE a.session_id=$1 AND a.kind='profiled_v1'
                  AND a.state#>>'{offer,manifest,bindings,project_id}' IS NOT NULL"
            }
        }
    }

    fn lock_query(self) -> &'static str {
        match self {
            Self::Manual => {
                "SELECT m.id FROM manual_agent_sessions m JOIN nodes n ON n.id=m.node_id
                WHERE m.id=$1 AND m.node_id=$2 AND n.revoked_at IS NULL FOR UPDATE OF m,n"
            }
            Self::Attempt => {
                "SELECT a.session_id FROM execution_attempts a
                WHERE a.attempt_id=$1 AND a.node_id=$2 AND a.kind='profiled_v1' FOR UPDATE OF a"
            }
        }
    }

    fn storage_queries(self) -> (&'static str, &'static str) {
        match self {
            Self::Manual => (
                "INSERT INTO manual_conversations(session_id,snapshot) VALUES($1,$2) ON CONFLICT DO NOTHING",
                "SELECT snapshot=$2 FROM manual_conversations WHERE session_id=$1",
            ),
            Self::Attempt => (
                "INSERT INTO attempt_conversations(attempt_id,snapshot) VALUES($1,$2) ON CONFLICT DO NOTHING",
                "SELECT snapshot=$2 FROM attempt_conversations WHERE attempt_id=$1",
            ),
        }
    }
}

async fn read_manual(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse> {
    read(s, headers, id, Subject::Manual).await
}

async fn read_attempt(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse> {
    read(s, headers, id, Subject::Attempt).await
}

async fn read(
    s: Service,
    headers: HeaderMap,
    id: Uuid,
    subject: Subject,
) -> Result<impl IntoResponse> {
    (s.owner)(headers).await?;
    let row: Option<(Uuid, Option<Value>)> = sqlx::query_as(subject.read_query())
        .bind(id)
        .fetch_optional(&s.pool)
        .await?;
    let (project, snapshot) = row.ok_or_else(missing)?;
    Ok((
        [("cache-control", "no-store")],
        Json(json!({"session_id":id,"project_id":project,"conversation":snapshot})),
    ))
}

async fn save_manual(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(snapshot): Json<Snapshot>,
) -> Result<Json<Value>> {
    save(s, headers, id, snapshot, Subject::Manual).await
}

async fn save_attempt(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(snapshot): Json<Snapshot>,
) -> Result<Json<Value>> {
    save(s, headers, id, snapshot, Subject::Attempt).await
}

async fn save(
    s: Service,
    headers: HeaderMap,
    id: Uuid,
    snapshot: Snapshot,
    subject: Subject,
) -> Result<Json<Value>> {
    let node = (s.node)(headers.clone()).await?;
    if snapshot.messages.is_empty()
        || snapshot.messages.len() > 1000
        || snapshot.messages.iter().any(|m| {
            m.text.trim().is_empty()
                || m.text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
        })
    {
        return Err(bad("Invalid conversation snapshot"));
    }
    let snapshot =
        serde_json::to_value(snapshot).map_err(|_| bad("Invalid conversation snapshot"))?;
    let mut tx = s.pool.begin().await?;
    // Match the profiled lifecycle's node-before-attempt lock order.
    if matches!(subject, Subject::Attempt)
        && sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM nodes WHERE id=$1 AND revoked_at IS NULL FOR UPDATE",
        )
        .bind(node)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        return Err(Error::enrolled_worker_required());
    }
    let session: Option<Uuid> = sqlx::query_scalar(subject.lock_query())
        .bind(id)
        .bind(node)
        .fetch_optional(&mut *tx)
        .await?;
    let session = session.ok_or_else(Error::enrolled_worker_required)?;
    if (s.node)(headers).await? != node {
        return Err(Error::enrolled_worker_required());
    }
    if matches!(subject, Subject::Attempt)
        && !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM profiled_stopped_receipts WHERE attempt_id=$1)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?
    {
        return Err(conflict("The attempt has no confirmed stop"));
    }
    let (insert, compare) = subject.storage_queries();
    let inserted = sqlx::query(insert)
        .bind(id)
        .bind(&snapshot)
        .execute(&mut *tx)
        .await?;
    if inserted.rows_affected() == 0 {
        let identical: bool = sqlx::query_scalar(compare)
            .bind(id)
            .bind(&snapshot)
            .fetch_one(&mut *tx)
            .await?;
        if !identical {
            return Err(conflict("A different conversation is already retained"));
        }
    }
    tx.commit().await?;
    Ok(Json(match subject {
        Subject::Manual => json!({"session_id":session}),
        Subject::Attempt => json!({"session_id":session,"attempt_id":id}),
    }))
}
