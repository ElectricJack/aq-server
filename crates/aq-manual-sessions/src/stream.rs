//! Disposable, fenced byte streams. Closing an attach never stops its agent.
use super::*;
mod browser;
mod worker;
use aq_session_protocol::{MAX_INPUT_BYTES, complete_terminal_input};
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
pub(super) struct Attach {
    client_id: Uuid,
    generation: u64,
    columns: Option<u16>,
    rows: Option<u16>,
    #[serde(default)]
    read_only: bool,
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
pub(super) async fn worker(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    let node = (s.node)(headers.clone()).await?;
    let (subject_node, live) = terminal_subject(&s, id).await?;
    if subject_node != node || !live {
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
        .on_upgrade(move |socket| {
            worker::Worker {
                service: s,
                headers,
                session: id,
                node,
                relay_id: relay.id,
            }
            .run(rx, socket)
        }))
}
pub(super) async fn browser(
    State(s): State<Service>,
    headers: HeaderMap,
    (Path(id), Query(a)): (Path<Uuid>, Query<Attach>),
    ws: WebSocketUpgrade,
) -> Result<Response> {
    origin(&s, &headers)?;
    let actor = (s.owner)(headers.clone()).await?;
    valid_id(a.client_id)?;
    let fixed = s.subject == Subject::Attempt;
    let mut attach = a.message(fixed)?;
    if !terminal_subject(&s, id).await?.1 {
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
    attach["connection_id"] = json!(connection);
    relay
        .tx
        .try_send(attach)
        .map_err(|_| conflict("Live terminal transport is busy"))?;
    screen.viewer = Some(Viewer {
        connection,
        generation: a.generation,
        tx,
    });
    drop(rt);
    if !a.read_only {
        attempt::controlled(&s, id, actor).await;
    }
    Ok(ws
        .max_message_size(8192)
        .max_frame_size(8192)
        .on_upgrade(move |socket| {
            Browser {
                s,
                headers,
                id,
                actor,
                a,
                connection,
                relay,
            }
            .run(rx, socket)
        }))
}
fn origin(s: &Service, headers: &HeaderMap) -> Result<()> {
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
    Ok(())
}
impl Attach {
    fn message(&self, fixed: bool) -> Result<Value> {
        if self.read_only && !fixed {
            return Err(conflict("Invalid viewer mode"));
        }
        let mut attach = json!({"kind":"attach","generation":self.generation});
        match (fixed, self.columns, self.rows) {
            (true, None, None) => attach["read_only"] = json!(self.read_only),
            (false, Some(c), Some(r)) if dimensions(c, r) => {
                attach["columns"] = json!(c);
                attach["rows"] = json!(r);
            }
            _ => return Err(conflict("Invalid viewer dimensions")),
        }
        Ok(attach)
    }
}
fn controls(screen: &terminal::Screen, actor: Uuid, a: &Attach) -> bool {
    screen.generation == a.generation && screen.controls(a.client_id, actor)
}
async fn permitted(s: &Service, id: Uuid) -> bool {
    if s.subject != Subject::Manual {
        return terminal_subject(s, id).await.is_ok_and(|(_, live)| live);
    }
    session(s,id).await.is_ok_and(|v|terminal::live(&v)) && sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM manual_agent_sessions m JOIN project_execution_bindings p ON p.project_id=m.project_id WHERE m.id=$1 AND p.binding->>'enabled'='true')").bind(id).fetch_one(&s.pool).await.unwrap_or(false)
}
type SocketSink = futures_util::stream::SplitSink<WebSocket, Message>;
async fn send(sink: &mut SocketSink, message: Message) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), sink.send(message)).await,
        Ok(Ok(()))
    )
}
fn ready(text: &str, connection: Uuid, fixed: bool) -> Option<Message> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value["kind"] != "ready" || value["connection_id"] != connection.to_string() {
        return None;
    }
    let mut reply = json!({"kind":"ready","server_time_ms":now()});
    if fixed {
        let columns = u16::try_from(value["columns"].as_u64()?).ok()?;
        let rows = u16::try_from(value["rows"].as_u64()?).ok()?;
        if !dimensions(columns, rows) {
            return None;
        }
        reply["columns"] = json!(columns);
        reply["rows"] = json!(rows);
    }
    Some(Message::Text(reply.to_string().into()))
}

fn browser_control(text: &str, fixed: bool, read_only: bool) -> Option<(Value, usize)> {
    let value: Value = serde_json::from_str(text).ok()?;
    match value["kind"].as_str()? {
        "repaint" if fixed => Some((json!({"kind":"repaint"}), 0)),
        "escape" if fixed && !read_only => Some((json!({"kind":"escape"}), 1)),
        // A manual pane takes the viewer's size. A task attempt pane keeps its
        // launch geometry until its current non-read-only controller resizes it.
        "resize" if !fixed || !read_only => {
            let c = u16::try_from(value["columns"].as_u64()?).ok()?;
            let r = u16::try_from(value["rows"].as_u64()?).ok()?;
            dimensions(c, r).then(|| (json!({"kind":"resize","columns":c,"rows":r}), 0))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_controls_repaint_without_input_and_resize_only_from_a_controller() {
        let resize = r#"{"kind":"resize","columns":80,"rows":8}"#;
        let forwarded = Some((json!({"kind":"resize","columns":80,"rows":8}), 0));
        // A controller of either pane kind may resize; a read-only attempt
        // viewer may not.
        assert_eq!(browser_control(resize, true, false), forwarded);
        assert_eq!(browser_control(resize, false, false), forwarded);
        assert!(browser_control(resize, true, true).is_none());
        assert_eq!(
            browser_control(r#"{"kind":"repaint"}"#, true, true),
            Some((json!({"kind":"repaint"}), 0))
        );
        assert_eq!(
            browser_control(r#"{"kind":"escape"}"#, true, false),
            Some((json!({"kind":"escape"}), 1))
        );
        assert!(browser_control(r#"{"kind":"repaint"}"#, false, false).is_none());
        assert!(browser_control(r#"{"kind":"escape"}"#, true, true).is_none());
    }

    #[test]
    fn resize_dimensions_are_bounded_and_extra_fields_are_not_forwarded() {
        for fixed in [true, false] {
            for (columns, rows, accepted) in [
                (2, 1, true),
                (500, 300, true),
                (1, 24, false),
                (0, 24, false),
                (501, 24, false),
                (80, 0, false),
                (80, 301, false),
                (65536, 24, false),
            ] {
                let text = json!({"kind":"resize","columns":columns,"rows":rows}).to_string();
                assert_eq!(
                    browser_control(&text, fixed, false).is_some(),
                    accepted,
                    "{columns}x{rows} fixed={fixed}"
                );
            }
            // Connection and generation are added by the server, never taken
            // from the browser frame.
            let spoofed = json!({"kind":"resize","columns":80,"rows":24,
                "connection_id":Uuid::from_u128(9),"generation":99})
            .to_string();
            assert_eq!(
                browser_control(&spoofed, fixed, false),
                Some((json!({"kind":"resize","columns":80,"rows":24}), 0))
            );
            let missing = json!({"kind":"resize","columns":80}).to_string();
            assert!(browser_control(&missing, fixed, false).is_none());
        }
    }

    #[test]
    fn attempt_ready_relays_only_the_bound_workers_pane_geometry() {
        let connection = Uuid::from_u128(1);
        let text = json!({"kind":"ready","connection_id":connection,"columns":120,"rows":40});
        let Some(Message::Text(reply)) = ready(&text.to_string(), connection, true) else {
            panic!("expected pane dimensions");
        };
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["columns"], 120);
        assert_eq!(reply["rows"], 40);
        assert!(ready(&text.to_string(), Uuid::from_u128(2), true).is_none());
    }
}
