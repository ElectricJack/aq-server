use super::*;
use axum::{
    Router,
    extract::{Path, Query, State},
    routing::{get, post},
};
#[derive(Default)]
pub(super) struct Screen {
    pub text: String,
    pub sequence: u64,
    pub seen: i64,
    pub generation: u64,
    pub input_sequence: u64,
    pub(super) controller: Option<(Uuid, Uuid, i64)>,
    pub commands: BTreeMap<Uuid, Command>,
}
pub(super) fn screen(runtime: &mut BTreeMap<Uuid, Screen>, id: Uuid) -> Result<&mut Screen> {
    if !runtime.contains_key(&id) && runtime.len() >= 512 {
        if let Some(old) = runtime
            .iter()
            .filter(|(_, s)| s.seen < now() - 15000)
            .min_by_key(|(_, s)| s.seen)
            .map(|(id, _)| *id)
        {
            runtime.remove(&old);
        } else {
            return Err(Error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Terminal replay is busy",
            ));
        }
    }
    Ok(runtime.entry(id).or_default())
}
#[derive(Clone, Serialize)]
pub(super) struct Command {
    pub sequence: u64,
    pub operation_id: Uuid,
    pub generation: u64,
    pub payload: Value,
    pub disposition: String,
    pub updated: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    client_id: Uuid,
    #[serde(default)]
    expected_generation: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    client_id: Uuid,
    #[serde(default)]
    after: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    operation_id: Uuid,
    client_id: Uuid,
    generation: u64,
    #[serde(default)]
    bytes: Option<Vec<u8>>,
    #[serde(default)]
    columns: Option<u32>,
    #[serde(default)]
    rows: Option<u32>,
}
pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/api/manual-agents/{id}/terminal", get(read))
        .route("/api/manual-agents/{id}/terminal/connect", post(connect))
        .route("/api/manual-agents/{id}/terminal/input", post(input))
        .route(
            "/api/manual-agents/{id}/terminal/receipts/{operation}",
            get(receipt),
        )
}
fn live(v: &Session) -> bool {
    v.status == "running"
        && v.desired_running
        && v.worker_seen_at_ms.is_some_and(|at| at > now() - 15000)
}
async fn connect(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(c): Json<Connect>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    valid_id(c.client_id)?;
    if !live(&session(&s, id).await?) {
        return Err(conflict("The agent is not ready to connect"));
    }
    let mut runtime = s.runtime.0.lock().await;
    let screen = screen(&mut runtime, id)?;
    if let Some(expected) = c.expected_generation
        && (screen.generation != expected
            || !screen
                .controller
                .is_some_and(|(client, a, _)| client == c.client_id && a == actor))
    {
        return Err(conflict("Terminal control changed; reconnect"));
    }
    if screen.commands.values().any(|cmd| {
        cmd.disposition == "unknown"
            || (cmd.disposition == "dispatched" && now() - cmd.updated > 30000)
    }) {
        return Err(conflict(
            "Input delivery remains uncertain; wait for the worker acknowledgement",
        ));
    }
    if !screen
        .controller
        .is_some_and(|(client, a, until)| client == c.client_id && a == actor && until > now())
    {
        screen.generation = screen
            .generation
            .checked_add(1)
            .ok_or_else(|| conflict("Terminal controller generation exhausted"))?;
        for cmd in screen.commands.values_mut() {
            if cmd.disposition == "queued" {
                cmd.disposition = "rejected".into();
            }
        }
    }
    screen.controller = Some((c.client_id, actor, now() + 15000));
    Ok(Json(
        json!({"generation":screen.generation,"expires_at_ms":now()+15000}),
    ))
}
async fn read(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<Read>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    let v = session(&s, id).await?;
    let mut runtime = s.runtime.0.lock().await;
    let screen = screen(&mut runtime, id)?;
    let controlling = live(&v)
        && screen
            .controller
            .is_some_and(|(client, a, until)| client == q.client_id && a == actor && until > now());
    Ok(Json(
        json!({"session":v,"sequence":screen.sequence,"snapshot":if screen.sequence>q.after {Some(&screen.text)}else{None},"generation":screen.generation,"controlling":controlling,"observed_at_ms":screen.seen,"server_time_ms":now()}),
    ))
}
async fn input(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(v): Json<Input>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    valid_id(v.operation_id)?;
    if !live(&session(&s, id).await?) {
        return Err(conflict("The agent is not connected"));
    }
    let payload = match (&v.bytes, v.columns, v.rows) {
        (Some(bytes), None, None) if !bytes.is_empty() && bytes.len() <= 4096 => {
            json!({"bytes":bytes})
        }
        (None, Some(columns), Some(rows))
            if (2..=500).contains(&columns) && (1..=300).contains(&rows) =>
        {
            json!({"columns":columns,"rows":rows})
        }
        _ => return Err(bad("Invalid terminal input or size")),
    };
    let mut runtime = s.runtime.0.lock().await;
    let screen = screen(&mut runtime, id)?;
    if screen.generation != v.generation
        || !screen
            .controller
            .is_some_and(|(c, a, t)| c == v.client_id && a == actor && t > now())
    {
        return Err(conflict("Terminal control changed; reconnect"));
    }
    if let Some(old) = screen.commands.get(&v.operation_id) {
        if old.generation == v.generation && old.payload == payload {
            return Ok(Json(json!({"disposition":old.disposition})));
        }
        return Err(conflict("Input operation identity is already used"));
    }
    screen.commands.retain(|_, cmd| {
        now() - cmd.updated < 30000
            || cmd.disposition == "queued"
            || cmd.disposition == "dispatched"
    });
    if screen.commands.len() >= 128
        || screen
            .commands
            .values()
            .filter(|c| c.disposition == "queued")
            .count()
            >= 16
    {
        return Err(conflict("Terminal input is busy"));
    }
    screen.input_sequence = screen
        .input_sequence
        .checked_add(1)
        .ok_or_else(|| conflict("Input sequence exhausted"))?;
    let sequence = screen.input_sequence;
    screen.commands.insert(
        v.operation_id,
        Command {
            sequence,
            operation_id: v.operation_id,
            generation: v.generation,
            payload,
            disposition: "queued".into(),
            updated: now(),
        },
    );
    Ok(Json(json!({"disposition":"queued"})))
}
async fn receipt(
    State(s): State<Service>,
    headers: HeaderMap,
    Path((id, op)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    session(&s, id).await?;
    let mut runtime = s.runtime.0.lock().await;
    let command = runtime
        .get_mut(&id)
        .and_then(|v| v.commands.get_mut(&op))
        .ok_or_else(missing)?;
    if command.disposition == "dispatched" && now() - command.updated > 30000 {
        command.disposition = "unknown".into();
    }
    Ok(Json(json!({"disposition":command.disposition})))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stopped_or_stale_sessions_never_enable_typing() {
        let mut v = Session {
            id: Uuid::new_v4(),
            node_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            spec: json!({}),
            status: "running".into(),
            stage: "Ready".into(),
            desired_running: true,
            created_at_ms: now(),
            updated_at_ms: now(),
            worker_seen_at_ms: Some(now()),
        };
        assert!(live(&v));
        v.worker_seen_at_ms = Some(now() - 16000);
        assert!(!live(&v));
        v.worker_seen_at_ms = Some(now());
        v.desired_running = false;
        assert!(!live(&v));
    }
}
