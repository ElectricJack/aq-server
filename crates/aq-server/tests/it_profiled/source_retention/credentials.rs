use super::*;

#[tokio::test]
async fn credential_rotation_after_observed_node_wait_cannot_create_transfer_and_new_key_replays() {
    let h = Harness::new().await;
    let op = Uuid::new_v4();
    let body = h.begin_body(op);
    let mut lock = h.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(h.identity.node_id)
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let app = h.app.clone();
    let token = h.token.clone();
    let input = body.clone();
    let blocked = tokio::spawn(async move {
        app.oneshot(
            Request::post("/v1/execution/sources/begin")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(2),async {loop {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'SELECT id FROM nodes WHERE id=%credential_hash=%FOR UPDATE')").fetch_one(&h.pool).await.unwrap();
        if waiting{break;}tokio::task::yield_now().await;
    }}).await.expect("must observe original credential's node lock wait");
    let replacement = new_node_credential();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(h.identity.node_id)
        .bind(verifier(&replacement))
        .execute(&mut *lock)
        .await
        .unwrap();
    lock.commit().await.unwrap();
    assert_eq!(blocked.await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM source_transfers")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let call = || {
        h.request(
            Request::post("/v1/execution/sources/begin").header("content-type", "application/json"),
            body.to_string().into_bytes(),
            Some(&replacement),
        )
    };
    let (code, first) = call().await;
    assert_eq!(code, StatusCode::OK, "{first}");
    assert_eq!(
        call().await.1,
        first,
        "new current credential replays original immutable operation"
    );
}

#[tokio::test]
async fn credential_rotation_during_import_keeps_uncertain_intent_and_current_key_recovers_same_op()
{
    // Synthetic socket owner isolates auth/recovery; actual Git retention is
    // independently qualified by phase_source_retention's explicit pinned cases.
    let h = Harness::new().await;
    h.upload_manifest().await;
    let (entered, resume, peer) = paused_retry_peer(&h);
    let pending = h.finalize();
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let replacement = new_node_credential();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(h.identity.node_id)
        .bind(verifier(&replacement))
        .execute(&h.pool)
        .await
        .unwrap();
    resume.add_permits(1);
    assert_eq!(pending.await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let pending_fact:Value=sqlx::query_scalar("SELECT jsonb_build_object('state',state,'dispatch',dispatch_intent,'receipt',receipt,'staging_released',staging_released) FROM source_transfers WHERE transfer_id=$1").bind(h.manifest.transfer_id).fetch_one(&h.pool).await.unwrap();
    assert_eq!(
        pending_fact,
        json!({"state":"import_pending","dispatch":true,"receipt":null,"staging_released":false})
    );
    assert!(
        h.dir
            .path()
            .join("git/artifacts/incoming")
            .join(h.manifest.artifact_id.to_string())
            .exists()
    );
    let body = h.status_body();
    let (code, recovered) = h
        .request(
            Request::post("/v1/execution/sources/finalize")
                .header("content-type", "application/json"),
            body.to_string().into_bytes(),
            Some(&replacement),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{recovered}");
    assert!(recovered["receipt"].is_object());
    assert_eq!(
        h.request(
            Request::post("/v1/execution/sources/finalize")
                .header("content-type", "application/json"),
            body.to_string().into_bytes(),
            Some(&replacement)
        )
        .await
        .1,
        recovered
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), peer)
        .await
        .unwrap()
        .unwrap();
}
