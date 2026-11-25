use super::*;

#[tokio::test]
async fn media_actual_retention_exact_bytes_original_actor_replay_and_content() {
    let (f, report, revision, owner, _root, actor) = setup().await;
    let id = retained(&owner, &actor, report.report_id, &revision).await;
    let status = owner.status(&actor, id).await.unwrap();
    let artifact = status.artifact.unwrap();
    assert_eq!(artifact.receipt.input.digest, media::Digest::of(&png()));
    assert_eq!(artifact.receipt.uploader_id.as_uuid(), actor.id);
    let again = owner.finalize(&actor, id).await.unwrap().artifact.unwrap();
    assert_eq!(artifact, again);
    assert_eq!(
        owner
            .reserve(&actor, report.report_id, &revision, &request(id))
            .await
            .unwrap()
            .artifact
            .unwrap(),
        artifact
    );
    let mut changed = request(id);
    changed.push(b' ');
    assert!(matches!(
        owner
            .reserve(&actor, report.report_id, &revision, &changed)
            .await,
        Err(Error::Conflict)
    ));
    let mut wrong = actor.clone();
    wrong.subject = Some("different".into());
    assert!(matches!(owner.status(&wrong, id).await, Err(Error::Access)));
    let content = owner
        .content(&actor, report.report_id, &revision, id, false)
        .await
        .unwrap();
    assert_eq!(
        media::Digest::of(content.bytes()),
        artifact.receipt.variants.render.digest
    );
    assert_eq!(content.receipt_digest, artifact.receipt_digest);
    let pool = f.db.pool().await;
    let (reserved,retained):(i64,i64)=sqlx::query_as("SELECT reserved_bytes,retained_bytes FROM result_media_budgets WHERE scope_key='installation'").fetch_one(&pool).await.unwrap();
    assert_eq!(reserved, 0);
    assert!(retained > 0 && retained <= 10_485_760);
    assert!(sqlx::query("UPDATE result_media_intents SET receipt_bytes=receipt_bytes||'x'::bytea WHERE operation_id=$1").bind(id).execute(&pool).await.is_err());
}

#[tokio::test]
async fn media_actual_quota_race_reserves_before_io_and_cleanup_keeps_history() {
    let (f, report, revision, owner, root, actor) = setup().await;
    let mut jobs = Vec::new();
    for _ in 0..7 {
        let owner = owner.clone();
        let actor = actor.clone();
        let revision = revision.clone();
        let report = report.report_id;
        jobs.push(tokio::spawn(async move {
            let id = Uuid::new_v4();
            owner.reserve(&actor, report, &revision, &request(id)).await
        }));
    }
    let mut accepted = 0;
    for job in jobs {
        match job.await.unwrap() {
            Ok(_) => accepted += 1,
            Err(Error::Capacity) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!(accepted, 6);
    assert_eq!(
        std::fs::read_dir(root.path().join("staging"))
            .unwrap()
            .count(),
        0
    );
    let pool = f.db.pool().await;
    let used: i64 = sqlx::query_scalar(
        "SELECT reserved_bytes FROM result_media_budgets WHERE scope_key='installation'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(used, 6 * 10_485_760);
    f.time.store(
        (NOW + media::MAX_UPLOAD_TTL_MS + 1) as i64,
        Ordering::SeqCst,
    );
    assert_eq!(owner.cleanup(16).await.unwrap(), 6);
    let (reserved,retained,history):(i64,i64,i64)=sqlx::query_as("SELECT reserved_bytes,retained_bytes,historical_intents FROM result_media_budgets WHERE scope_key='installation'").fetch_one(&pool).await.unwrap();
    assert_eq!((reserved, retained, history), (0, 0, 6));
}

#[tokio::test]
async fn media_actual_retention_expiry_waits_for_live_content_and_preserves_receipt() {
    let (f, report, revision, owner, root, actor) = setup().await;
    let id = retained(&owner, &actor, report.report_id, &revision).await;
    let original = owner
        .status(&actor, id)
        .await
        .unwrap()
        .artifact
        .unwrap()
        .receipt;
    let content = owner
        .content(&actor, report.report_id, &revision, id, false)
        .await
        .unwrap();
    f.time
        .store(original.retain_until_ms as i64, Ordering::SeqCst);
    assert_eq!(owner.cleanup(16).await.unwrap(), 0);
    assert!(matches!(
        owner
            .content(&actor, report.report_id, &revision, id, false)
            .await,
        Err(Error::Expired)
    ));
    drop(content);
    assert_eq!(owner.cleanup(16).await.unwrap(), 1);
    assert!(!root.path().join("objects").join(id.to_string()).exists());
    let page = owner
        .list(&actor, report.report_id, &revision)
        .await
        .unwrap();
    assert_eq!(page.items[0].receipt, original);
    assert_eq!(
        page.items[0].availability.state,
        media::AvailabilityState::Expired
    );
}

#[tokio::test]
async fn media_actual_live_physical_writer_blocks_expired_cleanup_budget_release() {
    let (f, report, revision, owner, root, actor) = setup().await;
    let id = Uuid::new_v4();
    owner
        .reserve(&actor, report.report_id, &revision, &request(id))
        .await
        .unwrap();
    let gate = Gate::new();
    owner.set_test_hook(Some(gate.hook("input.png"))).unwrap();
    let task = {
        let owner = owner.clone();
        let actor = actor.clone();
        tokio::spawn(async move { owner.put(&actor, id, png()).await })
    };
    gate.entered.notified().await;
    f.time.store(
        (NOW + media::MAX_UPLOAD_TTL_MS + 1) as i64,
        Ordering::SeqCst,
    );
    assert_eq!(owner.cleanup(16).await.unwrap(), 0);
    let pool = f.db.pool().await;
    let charge: i64 = sqlx::query_scalar(
        "SELECT reserved_bytes FROM result_media_budgets WHERE scope_key='installation'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(charge, 10_485_760);
    gate.release();
    assert!(matches!(task.await.unwrap(), Err(Error::Expired)));
    owner.set_test_hook(None).unwrap();
    assert_eq!(owner.cleanup(16).await.unwrap(), 1);
    assert!(!root.path().join("staging").join(id.to_string()).exists());
}

#[tokio::test]
async fn media_actual_cleanup_absence_durability_failure_keeps_charge_and_retries() {
    let (f, report, revision, owner, _root, actor) = setup().await;
    let id = Uuid::new_v4();
    owner
        .reserve(&actor, report.report_id, &revision, &request(id))
        .await
        .unwrap();
    f.time.store(
        (NOW + media::MAX_UPLOAD_TTL_MS + 1) as i64,
        Ordering::SeqCst,
    );
    owner
        .set_test_hook(Some(Arc::new(|name| {
            if name == "absence_durable" {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        })))
        .unwrap();
    assert_eq!(owner.cleanup(1).await.unwrap(), 0);
    let pool = f.db.pool().await;
    let (state, charge): (String, i64) = sqlx::query_as(
        "SELECT cleanup_state,reserved_bytes FROM result_media_intents WHERE operation_id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((state.as_str(), charge), ("blocked", 10_485_760));
    owner.set_test_hook(None).unwrap();
    assert_eq!(owner.cleanup(1).await.unwrap(), 1);
    let charge: i64 = sqlx::query_scalar(
        "SELECT reserved_bytes FROM result_media_budgets WHERE scope_key='installation'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(charge, 0);
}

#[tokio::test]
async fn media_actual_expired_pending_cleanup_releases_report_slots_preserving_tombstone() {
    let (f, report, revision, owner, _root, actor) = setup().await;
    let tombstone = retained(&owner, &actor, report.report_id, &revision).await;
    let receipt = owner
        .status(&actor, tombstone)
        .await
        .unwrap()
        .artifact
        .unwrap()
        .receipt;
    let mut at = receipt.retain_until_ms;
    f.time.store(at as i64, Ordering::SeqCst);
    assert_eq!(owner.cleanup(16).await.unwrap(), 1);
    let pool = f.db.pool().await;
    let tombstone_before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(i) FROM result_media_intents i WHERE operation_id=$1")
            .bind(tombstone)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tombstone_before["state"], "retained");
    assert_eq!(tombstone_before["cleanup_state"], "done");
    let mut abandoned = Vec::new();
    for (cycle, count) in [6, 2].into_iter().enumerate() {
        let mut ids = Vec::new();
        for index in 0..count {
            let id = Uuid::new_v4();
            owner
                .reserve(&actor, report.report_id, &revision, &request(id))
                .await
                .unwrap();
            if cycle == 0 && index < 2 {
                owner.put(&actor, id, png()).await.unwrap();
            }
            if cycle == 0 && index == 1 {
                leave_finalizing_intent(&owner, &actor, id).await;
            }
            ids.push(id);
        }
        let before = immutable_intent_fields(&pool, &ids).await;
        at += media::MAX_UPLOAD_TTL_MS + 1;
        f.time.store(at as i64, Ordering::SeqCst);
        assert_eq!(owner.cleanup(16).await.unwrap(), count);
        let after = immutable_intent_fields(&pool, &ids).await;
        assert_eq!(
            before, after,
            "original actor/raw request/source/policy/deadlines stay immutable"
        );
        let expired:i64=sqlx::query_scalar("SELECT count(*) FROM result_media_intents WHERE operation_id=ANY($1) AND state='expired' AND cleanup_state='done' AND reserved_bytes=0 AND retained_bytes=0").bind(&ids).fetch_one(&pool).await.unwrap();
        assert_eq!(expired, count as i64);
        abandoned.extend(ids);
    }
    assert_eq!(abandoned.len(), 8);
    let ninth = Uuid::new_v4();
    assert_eq!(
        owner
            .reserve(&actor, report.report_id, &revision, &request(ninth))
            .await
            .unwrap()
            .state,
        media::UploadState::Reserved
    );
    let tombstone_after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(i) FROM result_media_intents i WHERE operation_id=$1")
            .bind(tombstone)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        tombstone_before, tombstone_after,
        "retained tombstone/receipt is neither relabelled nor freed"
    );
    let counted:i64=sqlx::query_scalar("SELECT count(*) FROM result_media_intents WHERE report_id=$1 AND state IN ('reserved','received','finalizing','retained')").bind(report.report_id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        counted, 2,
        "one retained tombstone and the new ninth pending operation still count"
    );
    let (reserved,retained,history):(i64,i64,i64)=sqlx::query_as("SELECT reserved_bytes,retained_bytes,historical_intents FROM result_media_budgets WHERE scope_key='installation'").fetch_one(&pool).await.unwrap();
    assert_eq!((reserved, retained, history), (10_485_760, 0, 10));
}

async fn leave_finalizing_intent(owner: &Owner, actor: &User, id: Uuid) {
    owner
        .set_test_hook(Some(Arc::new(|name| {
            if name == "before_receipt_commit" {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        })))
        .unwrap();
    assert!(matches!(
        owner.finalize(actor, id).await,
        Err(Error::Storage)
    ));
    owner.set_test_hook(None).unwrap();
}

async fn immutable_intent_fields(pool: &sqlx::PgPool, ids: &[Uuid]) -> serde_json::Value {
    sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(i)-ARRAY['state','transition_at_ms','reserved_bytes','retained_bytes','lease_id','lease_expires_at_ms','cleanup_state','cleanup_generation','cleanup_completed_at_ms'] ORDER BY operation_id) FROM result_media_intents i WHERE operation_id=ANY($1)")
        .bind(ids)
        .fetch_one(pool)
        .await
        .unwrap()
}
