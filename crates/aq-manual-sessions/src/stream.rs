//! Disposable, fenced byte streams. Closing an attach never stops its agent.
use super::*;
use axum::{
    Router,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
#[derive(Clone)]
pub(super) struct Relay {
    id: Uuid,
    tx: mpsc::Sender<Value>,
}
pub(super) struct Viewer {
    connection: Uuid,
    generation: u64,
    tx: mpsc::Sender<Message>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attach {
    client_id: Uuid,
    generation: u64,
    columns: u16,
    rows: u16,
}
struct Browser {
    s: Service,
    headers: HeaderMap,
    id: Uuid,
    actor: Uuid,
    a: Attach,
    connection: Uuid,
    relay: Relay,
}
fn dimensions(c: u16, r: u16) -> bool {
    (2..=500).contains(&c) && (1..=300).contains(&r)
}
pub(super) fn human_routes() -> Router<Service> {
    Router::new().route("/api/manual-agents/{id}/terminal/live", get(browser))
}
pub(super) fn worker_routes() -> Router<Service> {
    Router::new().route("/v1/worker/manual-agents/{id}/terminal-stream", get(worker))
}
async fn worker(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    let node = (s.node)(headers.clone()).await?;
    let v = session(&s, id).await?;
    if v.node_id != node || !terminal::live(&v) {
        return Err(conflict("The enrolled worker does not own a live session"));
    }
    let mut rt = s.runtime.0.lock().await;
    if rt.values().filter(|s| s.relay.is_some()).count() >= 32
        && rt.get(&id).is_none_or(|s| s.relay.is_none())
    {
        return Err(conflict("Terminal stream capacity is occupied"));
    }
    let (tx, rx) = mpsc::channel(64);
    let relay = Relay {
        id: Uuid::new_v4(),
        tx,
    };
    let screen = terminal::screen(&mut rt, id)?;
    screen.relay = Some(relay.clone());
    screen.viewer = None;
    drop(rt);
    Ok(ws
        .max_message_size(131072)
        .max_frame_size(131072)
        .on_upgrade(move |socket| worker_socket(s, headers, id, node, relay.id, rx, socket)))
}
async fn browser(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(a): Query<Attach>,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    if !s.origin.as_ref().is_some_and(|(expected, dev)| {
        headers.get("origin").is_some_and(|v| {
            aq_human_http::origin_allowed(expected.trim_end_matches('/'), v.as_bytes(), *dev)
        })
    }) {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "Terminal origin is not allowed",
        ));
    }
    let actor = (s.owner)(headers.clone()).await?;
    valid_id(a.client_id)?;
    if !dimensions(a.columns, a.rows) || !terminal::live(&session(&s, id).await?) {
        return Err(conflict("The terminal is not ready"));
    }
    let mut rt = s.runtime.0.lock().await;
    let screen = terminal::screen(&mut rt, id)?;
    if !controls(screen, actor, &a) {
        return Err(conflict("Terminal control changed; reconnect"));
    }
    let relay = screen
        .relay
        .clone()
        .ok_or_else(|| conflict("Live terminal transport is connecting"))?;
    let connection = Uuid::new_v4();
    let (tx, rx) = mpsc::channel(64);
    relay.tx.try_send(json!({"kind":"attach","connection_id":connection,"generation":a.generation,"columns":a.columns,"rows":a.rows})).map_err(|_|conflict("Live terminal transport is busy"))?;
    screen.viewer = Some(Viewer {
        connection,
        generation: a.generation,
        tx,
    });
    drop(rt);
    Ok(ws
        .max_message_size(8192)
        .max_frame_size(8192)
        .on_upgrade(move |socket| {
            browser_socket(
                Browser {
                    s,
                    headers,
                    id,
                    actor,
                    a,
                    connection,
                    relay,
                },
                rx,
                socket,
            )
        }))
}
fn controls(screen: &terminal::Screen, actor: Uuid, a: &Attach) -> bool {
    screen.generation == a.generation
        && screen.controller.is_some_and(|(client, owner, until)| {
            client == a.client_id && owner == actor && until > now()
        })
}
async fn permitted(s: &Service, id: Uuid) -> bool {
    session(s,id).await.is_ok_and(|v|terminal::live(&v)) && sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM manual_agent_sessions m JOIN project_execution_bindings p ON p.project_id=m.project_id WHERE m.id=$1 AND p.binding->>'enabled'='true')").bind(id).fetch_one(&s.pool).await.unwrap_or(false)
}
async fn worker_socket(
    s: Service,
    headers: HeaderMap,
    id: Uuid,
    node: Uuid,
    relay_id: Uuid,
    mut rx: mpsc::Receiver<Value>,
    socket: WebSocket,
) {
    let (mut sink, mut source) = socket.split();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.tick().await;
    loop {
        tokio::select! {
         command=rx.recv()=>{let Some(c)=command else{break};if !matches!(tokio::time::timeout(Duration::from_secs(2),sink.send(Message::Text(c.to_string().into()))).await,Ok(Ok(()))){break;}}
         frame=source.next()=>{let Some(Ok(frame))=frame else{break};let mut rt=s.runtime.0.lock().await;let Some(screen)=rt.get_mut(&id) else{break};if screen.relay.as_ref().is_none_or(|r|r.id!=relay_id){break;}
          let Some(v)=screen.viewer.as_ref().filter(|v|v.generation==screen.generation&&screen.controller.is_some_and(|(_,_,until)|until>now())) else{continue};
          let output=match frame{
           Message::Binary(bytes) if bytes.len()>16 && bytes.len()<=16400 && bytes[..16]==*v.connection.as_bytes()=>Some(Message::Binary(bytes.slice(16..))),
           Message::Text(text)=>{match serde_json::from_str::<Value>(&text){Ok(value) if value["kind"]=="ready"&&value["connection_id"]==v.connection.to_string()=>Some(Message::Text(json!({"kind":"ready","server_time_ms":now()}).to_string().into())),_=>None}},
           Message::Close(_)=>break,_=>None};
          if let Some(output)=output && v.tx.try_send(output).is_err(){let connection=v.connection;let generation=v.generation;screen.viewer=None;if let Some(relay)=&screen.relay{let _=relay.tx.try_send(json!({"kind":"detach","connection_id":connection,"generation":generation}));}}
         }
         _=tick.tick()=>{if (s.node)(headers.clone()).await.ok()!=Some(node)||!permitted(&s,id).await{break;}let rt=s.runtime.0.lock().await;if rt.get(&id).and_then(|s|s.relay.as_ref()).is_none_or(|r|r.id!=relay_id){break;}drop(rt);if !matches!(tokio::time::timeout(Duration::from_secs(2),sink.send(Message::Ping(Vec::new().into()))).await,Ok(Ok(()))){break;}}
        }
    }
    let mut rt = s.runtime.0.lock().await;
    if let Some(screen) = rt.get_mut(&id)
        && screen.relay.as_ref().is_some_and(|r| r.id == relay_id)
    {
        screen.relay = None;
        screen.viewer = None;
    }
}
async fn browser_socket(context: Browser, mut rx: mpsc::Receiver<Message>, socket: WebSocket) {
    let Browser {
        s,
        headers,
        id,
        actor,
        a,
        connection,
        relay,
    } = context;
    let (mut sink, mut source) = socket.split();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut input_bytes = 0usize;
    loop {
        tokio::select! {
         output=rx.recv()=>{let Some(output)=output else{break};match tokio::time::timeout(Duration::from_secs(2),sink.send(output)).await{Ok(Ok(()))=>{},_=>break}}
         frame=source.next()=>{let Some(Ok(frame))=frame else{break};let payload=match frame{
          Message::Binary(bytes) if !bytes.is_empty()&&bytes.len()<=4096=>{input_bytes+=bytes.len();if input_bytes>65536{break;}json!({"kind":"input","bytes":bytes.to_vec()})},
          Message::Text(text)=>{let Ok(v)=serde_json::from_str::<Value>(&text) else{break};let (Some(c),Some(r))=(v["columns"].as_u64(),v["rows"].as_u64()) else{break};if v["kind"]!="resize"||c>500||r>300||!dimensions(c as u16,r as u16){break;}json!({"kind":"resize","columns":c,"rows":r})},
          Message::Close(_)=>break,Message::Ping(_)|Message::Pong(_)=>continue,_=>break};
          let rt=s.runtime.0.lock().await;let Some(screen)=rt.get(&id) else{break};if !controls(screen,actor,&a)||screen.viewer.as_ref().is_none_or(|v|v.connection!=connection)||screen.relay.as_ref().is_none_or(|r|r.id!=relay.id){break;}
          let mut payload=payload;payload["connection_id"]=json!(connection);payload["generation"]=json!(a.generation);if relay.tx.try_send(payload).is_err(){break;}
         }
         _=tick.tick()=>{input_bytes=0;if (s.owner)(headers.clone()).await.ok()!=Some(actor)||!permitted(&s,id).await{break;}let mut rt=s.runtime.0.lock().await;let Some(screen)=rt.get_mut(&id) else{break};if !controls(screen,actor,&a)||screen.viewer.as_ref().is_none_or(|v|v.connection!=connection)||screen.relay.as_ref().is_none_or(|r|r.id!=relay.id){break;}screen.controller=Some((a.client_id,actor,now()+15000));drop(rt);if !matches!(tokio::time::timeout(Duration::from_secs(2),sink.send(Message::Ping(Vec::new().into()))).await,Ok(Ok(()))){break;}}
        }
    }
    let mut rt = s.runtime.0.lock().await;
    if let Some(screen) = rt.get_mut(&id)
        && screen
            .viewer
            .as_ref()
            .is_some_and(|v| v.connection == connection)
    {
        screen.viewer = None;
        let _ = relay.tx.try_send(
            json!({"kind":"detach","connection_id":connection,"generation":a.generation}),
        );
    }
}
