//! Native provider messages are stored independently of the terminal archive.
use super::*;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::{get, post},
};

const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

pub(super) fn human_routes() -> Router<Service> {
    Router::new().route("/api/manual-agents/{id}/conversation", get(read))
}

pub(super) fn worker_routes() -> Router<Service> {
    Router::new()
        .route("/v1/worker/manual-agents/{id}/conversation", post(save))
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

async fn read(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse> {
    (s.owner)(headers).await?;
    let row: Option<(Uuid, Option<Value>)> = sqlx::query_as(
        "SELECT m.project_id,c.snapshot FROM manual_agent_sessions m
         LEFT JOIN manual_conversations c ON c.session_id=m.id WHERE m.id=$1",
    )
    .bind(id)
    .fetch_optional(&s.pool)
    .await?;
    let (project, snapshot) = row.ok_or_else(missing)?;
    Ok((
        [("cache-control", "no-store")],
        Json(json!({"session_id":id,"project_id":project,"conversation":snapshot})),
    ))
}

async fn save(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(snapshot): Json<Snapshot>,
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
    let authorized: Option<Uuid> = sqlx::query_scalar(
        "SELECT m.id FROM manual_agent_sessions m JOIN nodes n ON n.id=m.node_id
         WHERE m.id=$1 AND m.node_id=$2 AND n.revoked_at IS NULL FOR UPDATE OF m,n",
    )
    .bind(id)
    .bind(node)
    .fetch_optional(&mut *tx)
    .await?;
    if authorized.is_none() || (s.node)(headers).await? != node {
        return Err(Error::enrolled_worker_required());
    }
    let inserted = sqlx::query(
        "INSERT INTO manual_conversations(session_id,snapshot) VALUES($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(&snapshot)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        let identical: bool =
            sqlx::query_scalar("SELECT snapshot=$2 FROM manual_conversations WHERE session_id=$1")
                .bind(id)
                .bind(&snapshot)
                .fetch_one(&mut *tx)
                .await?;
        if !identical {
            return Err(conflict("A different conversation is already retained"));
        }
    }
    tx.commit().await?;
    Ok(Json(json!({"session_id":id})))
}
