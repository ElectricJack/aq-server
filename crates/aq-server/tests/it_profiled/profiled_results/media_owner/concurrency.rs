use super::*;

#[tokio::test]
async fn media_actual_subject_rebinding_during_io_denies_email_only_change_recovers() {
    let (f, report, revision, owner, _root, actor) = setup().await;
    let id = Uuid::new_v4();
    owner
        .reserve(&actor, report.report_id, &revision, &request(id))
        .await
        .unwrap();
    owner.put(&actor, id, png()).await.unwrap();
    let gate = Gate::new();
    owner
        .set_test_hook(Some(gate.hook("before_receipt_commit")))
        .unwrap();
    let task = {
        let owner = owner.clone();
        let actor = actor.clone();
        tokio::spawn(async move { owner.finalize(&actor, id).await })
    };
    gate.entered.notified().await;
    let pool = f.db.pool().await;
    sqlx::query("UPDATE users SET subject='replacement-principal' WHERE id=$1")
        .bind(actor.id)
        .execute(&pool)
        .await
        .unwrap();
    gate.release();
    assert!(matches!(task.await.unwrap(), Err(Error::Access)));
    sqlx::query("UPDATE users SET subject=$2,email='changed-display@localhost' WHERE id=$1")
        .bind(actor.id)
        .bind(&actor.subject)
        .execute(&pool)
        .await
        .unwrap();
    f.time.store((NOW + 30_001) as i64, Ordering::SeqCst);
    owner.set_test_hook(None).unwrap();
    let receipt = owner
        .finalize(&actor, id)
        .await
        .unwrap()
        .artifact
        .unwrap()
        .receipt;
    assert_eq!(receipt.uploader_id.as_uuid(), actor.id);
}

#[tokio::test]
async fn media_actual_list_probes_outside_sql_and_body_hash_is_checked_only_on_content() {
    let (f, report, revision, owner, root, actor) = setup().await;
    let id = retained(&owner, &actor, report.report_id, &revision).await;
    let path = root
        .path()
        .join("objects")
        .join(id.to_string())
        .join("render.png");
    let mut corrupt = std::fs::read(&path).unwrap();
    corrupt[20] ^= 1;
    std::fs::write(&path, &corrupt).unwrap();
    assert_eq!(
        owner
            .list(&actor, report.report_id, &revision)
            .await
            .unwrap()
            .items[0]
            .availability
            .state,
        media::AvailabilityState::Available
    );
    assert!(matches!(
        owner
            .content(&actor, report.report_id, &revision, id, false)
            .await,
        Err(Error::Integrity)
    ));
    let gate = Gate::new();
    owner.set_test_hook(Some(gate.hook("probe"))).unwrap();
    let task = {
        let owner = owner.clone();
        let actor = actor.clone();
        let revision = revision.clone();
        tokio::spawn(async move { owner.list(&actor, report.report_id, &revision).await })
    };
    gate.entered.notified().await;
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout='200ms'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(actor.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    gate.release();
    assert!(matches!(task.await.unwrap(), Err(Error::Access)));
}

async fn wait_for_node_waiters(pool: &sqlx::PgPool, minimum: i64) {
    tokio::time::timeout(std::time::Duration::from_millis(700),async{
        loop{
            let count:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'SELECT revoked_at IS NOT NULL FROM nodes%FOR UPDATE'")
                .fetch_one(pool).await.unwrap();
            if count>=minimum{return;}
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.expect("actual owner must reach the held node lock before timeout");
}

#[tokio::test]
async fn media_actual_status_node_before_intent_order_allows_concurrent_listing() {
    let (f, report, revision, owner, _root, actor) = setup().await;
    let id = retained(&owner, &actor, report.report_id, &revision).await;
    let pool = f.db.pool().await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let status = {
        let owner = owner.clone();
        let actor = actor.clone();
        tokio::spawn(async move { owner.status(&actor, id).await })
    };
    wait_for_node_waiters(&pool, 1).await;
    sqlx::query(
        "SELECT operation_id FROM result_media_intents WHERE operation_id=$1 FOR UPDATE NOWAIT",
    )
    .bind(id)
    .fetch_one(&mut *held)
    .await
    .expect("status waiting for Node must not already hold Intent");
    let listing = {
        let owner = owner.clone();
        let actor = actor.clone();
        let revision = revision.clone();
        tokio::spawn(async move { owner.list(&actor, report.report_id, &revision).await })
    };
    wait_for_node_waiters(&pool, 2).await;
    held.commit().await.unwrap();
    let status = status.await.unwrap().unwrap();
    let page = listing.await.unwrap().unwrap();
    assert_eq!(status.state, media::UploadState::Retained);
    assert_eq!(page.items[0].receipt, status.artifact.unwrap().receipt);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WriterPhase {
    Put,
    Finalize,
    LateFinalize,
}

#[tokio::test]
async fn media_actual_writer_post_io_node_before_intent_and_fresh_deadline_fences() {
    let (f, report, revision, owner, _root, actor) = setup().await;
    let pool = f.db.pool().await;
    for phase in [
        WriterPhase::Put,
        WriterPhase::Finalize,
        WriterPhase::LateFinalize,
    ] {
        let id = Uuid::new_v4();
        owner
            .reserve(&actor, report.report_id, &revision, &request(id))
            .await
            .unwrap();
        if phase != WriterPhase::Put {
            owner.put(&actor, id, png()).await.unwrap();
        }
        let gate = Gate::new();
        owner
            .set_test_hook(Some(gate.hook(if phase == WriterPhase::Put {
                "received_before_commit"
            } else {
                "before_receipt_commit"
            })))
            .unwrap();
        let write = {
            let owner = owner.clone();
            let actor = actor.clone();
            tokio::spawn(async move {
                if phase == WriterPhase::Put {
                    owner.put(&actor, id, png()).await
                } else {
                    owner.finalize(&actor, id).await
                }
            })
        };
        gate.entered.notified().await;
        let mut held = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(f.node.node_id)
            .fetch_one(&mut *held)
            .await
            .unwrap();
        gate.release();
        wait_for_node_waiters(&pool, 1).await;
        sqlx::query(
            "SELECT operation_id FROM result_media_intents WHERE operation_id=$1 FOR UPDATE NOWAIT",
        )
        .bind(id)
        .fetch_one(&mut *held)
        .await
        .expect("post-IO writer waiting for Node must not already hold Intent");
        let listing = {
            let owner = owner.clone();
            let actor = actor.clone();
            let revision = revision.clone();
            tokio::spawn(async move { owner.list(&actor, report.report_id, &revision).await })
        };
        wait_for_node_waiters(&pool, 2).await;
        if phase == WriterPhase::LateFinalize {
            f.time.store(
                (NOW + media::MAX_UPLOAD_TTL_MS + 1) as i64,
                Ordering::SeqCst,
            );
        }
        held.commit().await.unwrap();
        let result = write.await.unwrap();
        let page = listing.await.unwrap().unwrap();
        assert_eq!(page.binding.report_id.as_uuid(), report.report_id);
        owner.set_test_hook(None).unwrap();
        if phase == WriterPhase::LateFinalize {
            assert_expired_write(result, &pool, id).await;
        } else {
            assert_eq!(
                result.unwrap().state,
                if phase == WriterPhase::Put {
                    media::UploadState::Received
                } else {
                    media::UploadState::Retained
                }
            );
            if phase == WriterPhase::Put {
                owner.finalize(&actor, id).await.unwrap();
            }
        }
    }
}

async fn assert_expired_write(
    result: std::result::Result<media::UploadStatus, Error>,
    pool: &sqlx::PgPool,
    id: Uuid,
) {
    assert!(
        matches!(result, Err(Error::Expired)),
        "post-wait clock/deadline must be fresh"
    );
    let (state,charged,receipt):(String,i64,bool)=sqlx::query_as("SELECT state,reserved_bytes,receipt_bytes IS NOT NULL FROM result_media_intents WHERE operation_id=$1").bind(id).fetch_one(pool).await.unwrap();
    assert_eq!(
        (state.as_str(), charged, receipt),
        ("finalizing", 10_485_760, false)
    );
}

#[tokio::test]
async fn media_actual_status_uses_fresh_locked_flags_after_immutable_peek() {
    let (f, report, revision, owner, _root, actor) = setup().await;
    let id = retained(&owner, &actor, report.report_id, &revision).await;
    let pool = f.db.pool().await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let status = {
        let owner = owner.clone();
        let actor = actor.clone();
        tokio::spawn(async move { owner.status(&actor, id).await })
    };
    wait_for_node_waiters(&pool, 1).await;
    sqlx::query(
        "SELECT operation_id FROM result_media_intents WHERE operation_id=$1 FOR UPDATE NOWAIT",
    )
    .bind(id)
    .fetch_one(&mut *held)
    .await
    .unwrap();
    sqlx::query("UPDATE result_media_intents SET withheld_at_ms=$2 WHERE operation_id=$1")
        .bind(id)
        .bind(NOW as i64)
        .execute(&mut *held)
        .await
        .unwrap();
    held.commit().await.unwrap();
    let status = status.await.unwrap().unwrap();
    assert_eq!(
        status.artifact.unwrap().availability.state,
        media::AvailabilityState::Withheld
    );
}
