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
    save(s, headers, (Subject::Manual, id), snapshot).await
}

async fn save_attempt(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(snapshot): Json<Snapshot>,
) -> Result<Json<Value>> {
    save(s, headers, (Subject::Attempt, id), snapshot).await
}

async fn save(
    s: Service,
    headers: HeaderMap,
    (subject, id): (Subject, Uuid),
    snapshot: Snapshot,
) -> Result<Json<Value>> {
    let node = (s.node)(headers.clone()).await?;
    let snapshot = snapshot.into_value()?;
    let mut tx = s.pool.begin().await?;
    let session = subject.lock_session(&mut tx, id, node).await?;
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
    subject.store_snapshot(&mut tx, id, snapshot).await?;
    tx.commit().await?;
    Ok(Json(match subject {
        Subject::Manual => json!({"session_id":session}),
        Subject::Attempt => json!({"session_id":session,"attempt_id":id}),
    }))
}

impl Snapshot {
    fn into_value(self) -> Result<Value> {
        let valid =
            (1..=1000).contains(&self.messages.len()) && self.messages.iter().all(Message::valid);
        if !valid {
            return Err(bad("Invalid conversation snapshot"));
        }
        serde_json::to_value(self).map_err(|_| bad("Invalid conversation snapshot"))
    }
}
impl Message {
    fn valid(&self) -> bool {
        !self.text.trim().is_empty()
            && self
                .text
                .chars()
                .all(|c| !c.is_control() || matches!(c, '\n' | '\t'))
    }
}
impl Subject {
    async fn lock_session(
        self,
        conn: &mut sqlx::PgConnection,
        id: Uuid,
        node: Uuid,
    ) -> Result<Uuid> {
        // Match the profiled lifecycle's node-before-attempt lock order.
        if matches!(self, Subject::Attempt)
            && sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM nodes WHERE id=$1 AND revoked_at IS NULL FOR UPDATE",
            )
            .bind(node)
            .fetch_optional(&mut *conn)
            .await?
            .is_none()
        {
            return Err(Error::enrolled_worker_required());
        }
        let session: Option<Uuid> = sqlx::query_scalar(self.lock_query())
            .bind(id)
            .bind(node)
            .fetch_optional(&mut *conn)
            .await?;
        session.ok_or_else(Error::enrolled_worker_required)
    }
    async fn store_snapshot(
        self,
        conn: &mut sqlx::PgConnection,
        id: Uuid,
        snapshot: Value,
    ) -> Result<()> {
        let (insert, compare) = self.storage_queries();
        let inserted = sqlx::query(insert)
            .bind(id)
            .bind(&snapshot)
            .execute(&mut *conn)
            .await?;
        if inserted.rows_affected() == 0 {
            let identical: bool = sqlx::query_scalar(compare)
                .bind(id)
                .bind(&snapshot)
                .fetch_one(&mut *conn)
                .await?;
            if !identical {
                return Err(conflict("A different conversation is already retained"));
            }
        }
        Ok(())
    }
}
