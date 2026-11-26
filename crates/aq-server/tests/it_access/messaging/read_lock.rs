use super::*;
pub(super) async fn verify_expiry(body_chunk: bool) {
    let e = setup().await;
    let recipient = mailbox(&e, false);
    let m = message(recipient.clone());
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(m.clone()))
            .await
            .0,
        StatusCode::OK
    );
    let mut tx = e.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE mailbox_messages IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let app = e.app.clone();
    let token = e.token.clone();
    let (path, request) = if body_chunk {
        (
            "/v1/agent/messages/body",
            json!({"recipient":recipient,"message_id":m["message_id"],"offset":"0","max_bytes":512}),
        )
    } else {
        (
            "/v1/agent/messages/inbox",
            json!({"recipient":recipient,"after":"0","limit":1}),
        )
    };
    let pending = tokio::spawn(async move { send(&app, path, Some(&token), Some(request)).await });
    await_reader(&e.pool).await;
    e.time.store(1_010_000, std::sync::atomic::Ordering::SeqCst);
    tx.commit().await.unwrap();
    let response = pending.await.unwrap();
    assert_eq!(response.0, StatusCode::UNAUTHORIZED, "{response:?}");
    assert_eq!(response.1["code"], "session_expired");
    for field in ["messages", "body"] {
        assert!(response.1.get(field).is_none(), "{field}");
    }
}
async fn await_reader(pool: &sqlx::PgPool) {
    let blocked = async {
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event='relation' AND query LIKE '%mailbox_messages%')").fetch_one(pool).await.unwrap();
            if blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), blocked)
        .await
        .expect("authenticated read reached mailbox relation lock");
}
