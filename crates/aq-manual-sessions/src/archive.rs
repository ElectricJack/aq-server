use super::*;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    routing::{get, post},
};
use sqlx::Row;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

// Hold the permit across both SQL loading and CPU work, including after a
// caller disconnects or times out. This bounds resident archive copies.
static SEARCH_SLOTS: Semaphore = Semaphore::const_new(2);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
fn search_unavailable() -> Error {
    Error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Transcript search is temporarily unavailable",
    )
}

pub(super) fn human_routes() -> Router<Service> {
    Router::new()
        .route("/api/manual-agents/history", get(history))
        .route("/api/manual-agents/{id}/transcript", get(transcript))
        .route("/api/manual-agents/{id}/transcript/search", get(search))
}
pub(super) fn worker_routes() -> Router<Service> {
    Router::new()
        .route("/v1/worker/manual-agents/{id}/transcript", post(upload))
        .layer(DefaultBodyLimit::max(200 * 1024))
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
fn archive(row: Option<&sqlx::postgres::PgRow>, stopped: bool) -> Value {
    match row {
        None => {
            json!({"state":"not_recorded","coverage":"unknown","reason":null,"first_sequence":null,"last_sequence":null,"bytes":0,"updated_at_ms":null})
        }
        Some(r) => {
            let last: i64 = r.get("last_sequence");
            let finalized: bool = r.get("finalized");
            json!({"state":if finalized {"complete"} else if stopped {"finalizing"} else {"recording"},"coverage":r.get::<String,_>("coverage"),"reason":r.get::<Option<String>,_>("reason"),"first_sequence":if last>0 {Some(1)} else {None},"last_sequence":if last>0 {Some(last)} else {None},"bytes":r.get::<i64,_>("bytes"),"updated_at_ms":r.get::<i64,_>("updated_at_ms")})
        }
    }
}
async fn history_session(s: &Service, id: Uuid) -> Result<Value> {
    let row = session(s, id).await?;
    let a = sqlx::query("SELECT * FROM manual_transcript_archives WHERE session_id=$1")
        .bind(id)
        .fetch_optional(&s.pool)
        .await?;
    let mut v = serde_json::to_value(&row).map_err(|_| bad("Invalid retained session"))?;
    v["archive"] = archive(
        a.as_ref(),
        matches!(row.status.as_str(), "stopped" | "failed"),
    );
    Ok(v)
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
    let rows: Vec<(Uuid,i64)> = sqlx::query_as("SELECT id,created_at_ms FROM manual_agent_sessions WHERE ($1::uuid IS NULL OR project_id=$1) AND ($2::text IS NULL OR ($2='active' AND stopped_at_ms IS NULL) OR ($2='ended' AND stopped_at_ms IS NOT NULL)) AND ($3::text IS NULL OR spec->>'provider'=$3) AND ($4::text IS NULL OR spec->>'model'=$4) AND ($5::bigint IS NULL OR created_at_ms >= $5) AND ($6::bigint IS NULL OR created_at_ms < $6) AND ($7::text IS NULL OR position(lower($7) in lower(spec::text || ' ' || id::text))>0) AND ($8::bigint IS NULL OR (created_at_ms,id)<($8,$9::uuid)) ORDER BY created_at_ms DESC,id DESC LIMIT $10")
        .bind(q.project_id).bind(q.status).bind(q.provider).bind(q.model).bind(q.created_from_ms).bind(q.created_until_ms).bind(q.q).bind(cursor.map(|x|x.0)).bind(cursor.map(|x|x.1)).bind(n+1).fetch_all(&s.pool).await?;
    let more = rows.len() > n as usize;
    let mut sessions = Vec::new();
    for (id, _) in rows.iter().take(n as usize) {
        sessions.push(history_session(&s, *id).await?);
    }
    let next = if more {
        rows.get(n as usize - 1).map(|(id, t)| format!("{t}:{id}"))
    } else {
        None
    };
    Ok(Json(
        json!({"sessions":sessions,"next_cursor":next,"server_time_ms":now()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadQuery {
    after: Option<i64>,
    limit: Option<u32>,
    through: Option<i64>,
}
async fn transcript(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<ReadQuery>,
) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    let n = limit(q.limit, 32, 64)?;
    let after = q.after.unwrap_or(0);
    if after < 0 || q.through.is_some_and(|x| x < 0) {
        return Err(bad("Invalid sequence"));
    }
    let sess = history_session(&s, id).await?;
    let high = sess["archive"]["last_sequence"].as_i64().unwrap_or(0);
    if q.through.is_some_and(|through| through > high) {
        return Err(bad("Snapshot sequence is unavailable"));
    }
    let through = q.through.unwrap_or(high);
    let rows: Vec<(i64,String)>=sqlx::query_as("SELECT sequence,text FROM manual_transcript_chunks WHERE session_id=$1 AND sequence>$2 AND sequence<=$3 ORDER BY sequence LIMIT $4").bind(id).bind(after).bind(through).bind(n+1).fetch_all(&s.pool).await?;
    let mut chunks = Vec::new();
    let mut bytes = 0;
    let mut next = after;
    for (seq, text) in rows.iter().take(n as usize) {
        if bytes + text.len() > 256 * 1024 {
            break;
        }
        bytes += text.len();
        next = *seq;
        chunks.push(json!({"sequence":seq,"text":text}));
    }
    Ok(Json(
        json!({"session":sess,"chunks":chunks,"next_after":next,"has_more":next<through,"through_sequence":through,"server_time_ms":now()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    q: String,
    after: Option<usize>,
    limit: Option<u32>,
    through: Option<i64>,
}
async fn search(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    let n = limit(q.limit, 50, 100)? as usize;
    if q.q.is_empty() || q.q.chars().count() > 200 || q.through.is_some_and(|x| x < 0) {
        return Err(bad("Invalid search"));
    }
    let sess = history_session(&s, id).await?;
    let high = sess["archive"]["last_sequence"].as_i64().unwrap_or(0);
    if q.through.is_some_and(|through| through > high) {
        return Err(bad("Snapshot sequence is unavailable"));
    }
    let through = q.through.unwrap_or(high);
    let permit = tokio::time::timeout(SEARCH_TIMEOUT, SEARCH_SLOTS.acquire())
        .await
        .map_err(|_| search_unavailable())?
        .map_err(|_| search_unavailable())?;
    let mut tx = s.pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    let rows:Vec<(i64,String)>=sqlx::query_as("SELECT sequence,text FROM manual_transcript_chunks WHERE session_id=$1 AND sequence<=$2 ORDER BY sequence").bind(id).bind(through).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let deadline = Instant::now() + SEARCH_TIMEOUT;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        find_matches(rows, q.q, q.after, n, deadline)
    });
    let (found, more, next) = tokio::time::timeout(SEARCH_TIMEOUT, task)
        .await
        .map_err(|_| search_unavailable())?
        .map_err(|_| search_unavailable())??;
    Ok(Json(
        json!({"matches":found,"next_after":next,"has_more":more,"through_sequence":through,"server_time_ms":now()}),
    ))
}

fn find_matches(
    rows: Vec<(i64, String)>,
    query: String,
    after: Option<usize>,
    n: usize,
    deadline: Instant,
) -> Result<(Vec<Value>, bool, Option<u64>)> {
    let mut text = String::new();
    let mut starts = Vec::new();
    for (seq, chunk) in rows {
        starts.push((text.len(), seq));
        text.push_str(&chunk);
    }
    // Fold once, then map only candidate matches back to original byte ranges.
    // This keeps memory proportional to the archive rather than one mapping per byte.
    let folded: String = text.chars().flat_map(char::to_lowercase).collect();
    let needle: String = query.chars().flat_map(char::to_lowercase).collect();
    let mut found = Vec::new();
    let mut cursor = 0;
    let mut chars = text.char_indices().peekable();
    let mut folded_pos = 0;
    let mut last_offset = None;
    while cursor < folded.len() {
        if Instant::now() >= deadline {
            return Err(search_unavailable());
        }
        let Some(rel) = folded[cursor..].find(&needle) else {
            break;
        };
        let start = cursor + rel;
        let end = start + needle.len();
        while let Some(&(_, ch)) = chars.peek() {
            let width: usize = ch.to_lowercase().map(char::len_utf8).sum();
            if folded_pos + width > start {
                break;
            }
            folded_pos += width;
            chars.next();
        }
        let Some(&(offset, _)) = chars.peek() else {
            break;
        };
        let mut scan = chars.clone();
        let mut scan_pos = folded_pos;
        let mut finish = offset;
        while scan_pos < end {
            let Some((pos, ch)) = scan.next() else { break };
            scan_pos += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
            finish = pos + ch.len_utf8();
        }
        if after.is_none_or(|a| offset > a) && last_offset != Some(offset) {
            let seq = starts
                .iter()
                .rev()
                .find(|(p, _)| *p <= offset)
                .map_or(0, |(_, s)| *s);
            let mut lo = offset.saturating_sub(80);
            while !text.is_char_boundary(lo) {
                lo += 1;
            }
            let mut hi = (finish + 80).min(text.len());
            while !text.is_char_boundary(hi) {
                hi -= 1;
            }
            found.push(json!({"offset":offset,"length":finish-offset,"sequence":seq,"excerpt":&text[lo..hi]}));
            last_offset = Some(offset);
            if found.len() > n {
                break;
            }
        }
        cursor = start + folded[start..].chars().next().unwrap().len_utf8();
    }
    let more = found.len() > n;
    found.truncate(n);
    let next = found.last().and_then(|v| v["offset"].as_u64());
    Ok((found, more, next))
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Upload {
    Chunk(Chunk),
    Final(Final),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chunk {
    sequence: i64,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Final {
    final_sequence: i64,
    coverage: String,
    reason: Option<String>,
}
async fn upload(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(v): Json<Upload>,
) -> std::result::Result<Json<Value>, Response> {
    upload_inner(s, headers, id, v).await.map(Json)
}
async fn upload_inner(
    s: Service,
    headers: HeaderMap,
    id: Uuid,
    v: Upload,
) -> std::result::Result<Value, Response> {
    let node = (s.node)(headers.clone())
        .await
        .map_err(IntoResponse::into_response)?;
    match &v {
        Upload::Chunk(c)
            if c.sequence < 1
                || c.text.len() > 32768
                || c.text.is_empty()
                || c.text
                    .chars()
                    .any(|ch| ch.is_ascii_control() && ch != '\n' && ch != '\t') =>
        {
            return Err(bad("Invalid chunk").into_response());
        }
        Upload::Final(f)
            if f.final_sequence < 0
                || !["complete", "partial"].contains(&f.coverage.as_str())
                || f.reason.as_ref().is_some_and(|r| {
                    r.chars().count() > 512 || r.chars().any(char::is_control)
                })
                || (f.coverage == "complete" && f.reason.is_some()) =>
        {
            return Err(bad("Invalid finalization").into_response());
        }
        _ => (),
    }
    let mut tx = s
        .pool
        .begin()
        .await
        .map_err(|e| Error::from(e).into_response())?;
    let owner: Option<(Uuid,Uuid,bool)>=sqlx::query_as("SELECT m.node_id,m.project_id,m.stopped_at_ms IS NOT NULL FROM manual_agent_sessions m JOIN nodes n ON n.id=m.node_id WHERE m.id=$1 AND m.node_id=$2 AND n.revoked_at IS NULL FOR UPDATE OF m,n").bind(id).bind(node).fetch_optional(&mut *tx).await.map_err(|e|Error::from(e).into_response())?;
    let Some((_, project, stopped)) = owner else {
        return Err(Error::enrolled_worker_required().into_response());
    };
    if (s.node)(headers)
        .await
        .map_err(IntoResponse::into_response)?
        != node
    {
        return Err(Error::enrolled_worker_required().into_response());
    }
    sqlx::query("INSERT INTO manual_transcript_archives(session_id,node_id,project_id,updated_at_ms) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING").bind(id).bind(node).bind(project).bind(now()).execute(&mut *tx).await.map_err(|e|Error::from(e).into_response())?;
    let row =
        sqlx::query("SELECT * FROM manual_transcript_archives WHERE session_id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| Error::from(e).into_response())?;
    if row.get::<Uuid, _>("node_id") != node || row.get::<Uuid, _>("project_id") != project {
        return Err(Error::enrolled_worker_required().into_response());
    }
    let mut last: i64 = row.get("last_sequence");
    let bytes: i64 = row.get("bytes");
    let finalized: bool = row.get("finalized");
    let reject = |last: i64| {
        (StatusCode::CONFLICT,Json(json!({"error":"manual_transcript_conflict","message":"Transcript sequence or payload conflict","next_sequence":last+1}))).into_response()
    };
    let is_final = matches!(&v, Upload::Final(_));
    let mut retry_receipt: Option<(i64, String)> = None;
    match v {
        Upload::Chunk(c) => {
            if c.sequence <= last {
                let old: Option<(String, String)> = sqlx::query_as(
                    "SELECT text,ack_state FROM manual_transcript_chunks WHERE session_id=$1 AND sequence=$2",
                )
                .bind(id).bind(c.sequence).fetch_optional(&mut *tx).await
                .map_err(|e| Error::from(e).into_response())?;
                match old {
                    Some((text, state)) if text == c.text => {
                        retry_receipt = Some((c.sequence, state))
                    }
                    _ => return Err(reject(last)),
                }
            } else {
                if finalized
                    || c.sequence != last + 1
                    || bytes + c.text.len() as i64 > 64 * 1024 * 1024
                {
                    return Err(reject(last));
                }
                sqlx::query("INSERT INTO manual_transcript_chunks(session_id,sequence,text,ack_state) VALUES($1,$2,$3,$4)").bind(id).bind(c.sequence).bind(&c.text).bind(if stopped {"finalizing"} else {"recording"}).execute(&mut *tx).await.map_err(|e|Error::from(e).into_response())?;
                last = c.sequence;
                sqlx::query("UPDATE manual_transcript_archives SET last_sequence=$2,bytes=$3,updated_at_ms=$4 WHERE session_id=$1").bind(id).bind(last).bind(bytes+c.text.len() as i64).bind(now()).execute(&mut *tx).await.map_err(|e|Error::from(e).into_response())?;
            }
        }
        Upload::Final(f) => {
            if f.final_sequence != last
                || (finalized
                    && (row.get::<String, _>("coverage") != f.coverage
                        || row.get::<Option<String>, _>("reason") != f.reason))
            {
                return Err(reject(last));
            }
            if !finalized {
                sqlx::query("UPDATE manual_transcript_archives SET finalized=true,coverage=$2,reason=$3,updated_at_ms=$4 WHERE session_id=$1").bind(id).bind(f.coverage).bind(f.reason).bind(now()).execute(&mut *tx).await.map_err(|e|Error::from(e).into_response())?;
            }
        }
    }
    let state = if finalized || is_final {
        "complete"
    } else if stopped {
        "finalizing"
    } else {
        "recording"
    };
    tx.commit()
        .await
        .map_err(|e| Error::from(e).into_response())?;
    let (accepted, state) = retry_receipt.unwrap_or_else(|| (last, state.to_owned()));
    Ok(json!({"accepted_through":accepted,"next_sequence":accepted+1,"archive_state":state}))
}
