use super::*;

/// Synthetic central aq-git peer: answers `candidate.create` and `publish`
/// in order, after checking the intent was committed before the socket call.
pub(crate) async fn git_peer(
    pool: sqlx::PgPool,
    socket: std::path::PathBuf,
    replies: Vec<Value>,
) -> tokio::task::JoinHandle<Vec<Value>> {
    let peer = git_peer_cancelling(pool, socket, replies, None).await;
    tokio::spawn(async move { peer.await.unwrap().0 })
}

/// An owner cancel issued by the peer while it holds request `index` (before
/// replying): the actual B3 cancellation entry point, under its own locks.
type CancelAt = Option<(usize, aq_server::AppState, Uuid)>;

pub(super) async fn git_peer_cancelling(
    pool: sqlx::PgPool,
    socket: std::path::PathBuf,
    replies: Vec<Value>,
    cancel_at: CancelAt,
) -> tokio::task::JoinHandle<(Vec<Value>, Option<String>)> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        let mut seen = Vec::new();
        let mut cancelled = None;
        for (index, mut reply) in replies.into_iter().enumerate() {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let id = Uuid::parse_str(req["operation_id"].as_str().unwrap()).unwrap();
            let stored: Vec<u8> =
                sqlx::query_scalar("SELECT request FROM delivery_intents WHERE operation_id=$1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(stored, line.as_bytes(), "intent committed before the call");
            if let Some((at, state, attempt)) = &cancel_at
                && *at == index
            {
                let receipt = aq_server::profiled_execution::cancellation::cancel(
                    state,
                    *attempt,
                    Uuid::new_v4(),
                    "owner cancel while publishing",
                )
                .await
                .unwrap();
                cancelled = Some(json!(receipt.disposition).as_str().unwrap().to_owned());
            }
            if reply["result"]["candidate_id"] == "OP" {
                reply["result"]["candidate_id"] = req["operation_id"].clone();
                reply["result"]["retention_handle"] = req["operation_id"].clone();
            }
            reply["protocol_version"] = json!(1);
            reply["request_id"] = req["request_id"].clone();
            reply["operation_id"] = req["operation_id"].clone();
            reader
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            seen.push(req);
        }
        (seen, cancelled)
    })
}

/// Counts every connection to the central socket; nothing may arrive.
pub(crate) fn silent_git(socket: &std::path::Path) -> Arc<AtomicUsize> {
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    tokio::spawn(async move {
        while listener.accept().await.is_ok() {
            c.fetch_add(1, Ordering::SeqCst);
        }
    });
    count
}
