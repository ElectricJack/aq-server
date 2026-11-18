use super::*;
use axum::{
    Router,
    extract::{Query, State},
    routing::get,
};

pub(super) fn human_routes() -> Router<Service> {
    Router::new().route("/api/manual-agents/history", get(history))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    project_id: Option<Uuid>,
    status: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    created_from_ms: Option<i64>,
    created_until_ms: Option<i64>,
    q: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
}
fn limit(v: Option<u32>, default: u32, max: u32) -> Result<i64> {
    let n = v.unwrap_or(default);
    if n == 0 || n > max {
        return Err(bad("Invalid page limit"));
    }
    Ok(n as i64)
}
async fn history(
    State(s): State<Service>,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    let n = limit(q.limit, 50, 100)?;
    if q.status
        .as_deref()
        .is_some_and(|v| !["active", "ended"].contains(&v))
        || [&q.q, &q.provider, &q.model]
            .into_iter()
            .flatten()
            .any(|v| v.chars().count() > 200)
    {
        return Err(bad("Invalid history filter"));
    }
    let cursor = q
        .cursor
        .as_ref()
        .map(|v| {
            let (t, id) = v.split_once(':').ok_or_else(|| bad("Invalid cursor"))?;
            Ok::<_, Error>((
                t.parse::<i64>().map_err(|_| bad("Invalid cursor"))?,
                Uuid::parse_str(id).map_err(|_| bad("Invalid cursor"))?,
            ))
        })
        .transpose()?;
    let rows: Vec<(Uuid,i64,bool)> = sqlx::query_as("SELECT id,created_at_ms,EXISTS(SELECT 1 FROM manual_conversations c WHERE c.session_id=manual_agent_sessions.id) FROM manual_agent_sessions WHERE ($1::uuid IS NULL OR project_id=$1) AND ($2::text IS NULL OR ($2='active' AND stopped_at_ms IS NULL) OR ($2='ended' AND stopped_at_ms IS NOT NULL)) AND ($3::text IS NULL OR spec->>'provider'=$3) AND ($4::text IS NULL OR spec->>'model'=$4) AND ($5::bigint IS NULL OR created_at_ms >= $5) AND ($6::bigint IS NULL OR created_at_ms < $6) AND ($7::text IS NULL OR position(lower($7) in lower(spec::text || ' ' || id::text))>0) AND ($8::bigint IS NULL OR (created_at_ms,id)<($8,$9::uuid)) ORDER BY created_at_ms DESC,id DESC LIMIT $10")
        .bind(q.project_id).bind(q.status).bind(q.provider).bind(q.model).bind(q.created_from_ms).bind(q.created_until_ms).bind(q.q).bind(cursor.map(|x|x.0)).bind(cursor.map(|x|x.1)).bind(n+1).fetch_all(&s.pool).await?;
    let more = rows.len() > n as usize;
    let mut sessions = Vec::new();
    for (id, _, conversation_available) in rows.iter().take(n as usize) {
        let mut row = serde_json::to_value(session(&s, *id).await?)
            .map_err(|_| bad("Invalid retained session"))?;
        row["conversation_available"] = json!(conversation_available);
        sessions.push(row);
    }
    let next = if more {
        rows.get(n as usize - 1)
            .map(|(id, t, _)| format!("{t}:{id}"))
    } else {
        None
    };
    Ok(Json(
        json!({"sessions":sessions,"next_cursor":next,"server_time_ms":now()}),
    ))
}
