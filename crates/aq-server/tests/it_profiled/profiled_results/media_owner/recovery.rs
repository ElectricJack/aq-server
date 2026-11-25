use super::*;

#[tokio::test]
async fn media_actual_interrupted_publish_reuses_one_generation_and_root_swaps_fail() {
    let (_f, report, revision, owner, root, actor) = setup().await;
    let id = Uuid::new_v4();
    owner
        .reserve(&actor, report.report_id, &revision, &request(id))
        .await
        .unwrap();
    owner.put(&actor, id, png()).await.unwrap();
    owner
        .set_test_hook(Some(Arc::new(|name| {
            if name == "published" {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        })))
        .unwrap();
    assert!(matches!(
        owner.finalize(&actor, id).await,
        Err(Error::Storage)
    ));
    assert!(root.path().join("objects").join(id.to_string()).is_dir());
    assert_eq!(
        std::fs::read_dir(root.path().join("staging"))
            .unwrap()
            .count(),
        0
    );
    // A separate root/storage binding cannot silently take over this database.
    let other = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    assert!(matches!(
        Owner::open(_f.state.clone(), other.path(), Uuid::new_v4()).await,
        Err(Error::Storage)
    ));
    let moved = root.path().with_extension("moved");
    std::fs::rename(root.path(), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, root.path()).unwrap();
    assert!(owner.cleanup(1).await.is_err());
    std::fs::remove_file(root.path()).unwrap();
    std::fs::rename(moved, root.path()).unwrap();
}

#[tokio::test]
async fn media_actual_sql_refuses_unleased_finalizing_and_backdated_expired_receipt() {
    let (f, report, revision, owner, _root, actor) = setup().await;
    let id = Uuid::new_v4();
    owner
        .reserve(&actor, report.report_id, &revision, &request(id))
        .await
        .unwrap();
    let received = owner.put(&actor, id, png()).await.unwrap();
    let pool = f.db.pool().await;
    assert!(
        sqlx::query("UPDATE result_media_intents SET state='finalizing' WHERE operation_id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .is_err()
    );
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
    f.time.store((NOW + 30_001) as i64, Ordering::SeqCst);
    gate.release();
    assert!(matches!(task.await.unwrap(), Err(Error::Conflict)));
    // Deliberately forged negative vector only: no SQL receipt below is ever
    // accepted as positive file-retention evidence.
    let image = media::prepare_png(&png()).unwrap();
    let receipt = media::Receipt {
        schema_version: 1,
        artifact_id: media::Id::new(id).unwrap(),
        binding: received.binding,
        uploader_id: media::Id::new(actor.id).unwrap(),
        input: received.input,
        capture: None,
        rendering_policy: media::RenderingPolicy::Rgba8SrgbV1,
        variants: media::Variants {
            render: image.render().descriptor().clone(),
            preview: image.preview().descriptor().clone(),
        },
        retained_at_ms: NOW + 1,
        retain_until_ms: received.retain_until_ms,
        clip: None,
    };
    let bytes = media::canonical_bytes(&receipt).unwrap();
    assert!(sqlx::query("UPDATE result_media_intents SET state='retained',receipt_bytes=$2,receipt_digest=$3,reserved_bytes=0,retained_bytes=100000,lease_id=NULL,lease_expires_at_ms=NULL,transition_at_ms=$4 WHERE operation_id=$1").bind(id).bind(&bytes).bind(media::Digest::of(&bytes).as_str()).bind((NOW+30_001) as i64).execute(&pool).await.is_err());
    owner.set_test_hook(None).unwrap();
    assert_eq!(
        owner.finalize(&actor, id).await.unwrap().state,
        media::UploadState::Retained
    );
}

#[tokio::test]
async fn media_actual_fsync_rename_precommit_interruptions_recover_exact_one_generation() {
    let (f, report, revision, owner, root, actor) = setup().await;
    let mut at = NOW;
    for (index, point) in [
        "input.png",
        "render.png",
        "preview.png",
        "renamed",
        "published",
        "before_receipt_commit",
    ]
    .into_iter()
    .enumerate()
    {
        let id = Uuid::new_v4();
        owner
            .reserve(&actor, report.report_id, &revision, &request(id))
            .await
            .unwrap();
        let fired = interrupt_once(&owner, point);
        if point == "input.png" {
            assert!(matches!(
                owner.put(&actor, id, png()).await,
                Err(Error::Storage)
            ));
        } else {
            owner.put(&actor, id, png()).await.unwrap();
            assert!(matches!(
                owner.finalize(&actor, id).await,
                Err(Error::Storage)
            ));
        }
        assert!(fired.load(Ordering::SeqCst));
        let crossed = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = crossed.clone();
        owner
            .set_test_hook(Some(Arc::new(move |name| {
                recorded.lock().unwrap().push(name.to_owned());
                Ok(())
            })))
            .unwrap();
        at += 30_001;
        f.time.store(at as i64, Ordering::SeqCst);
        if point == "input.png" {
            owner.put(&actor, id, png()).await.unwrap();
        }
        let retained = owner.finalize(&actor, id).await.unwrap();
        assert_durable_before_receipt(&crossed.lock().unwrap());
        owner.set_test_hook(None).unwrap();
        assert_eq!(retained.artifact.unwrap().receipt.artifact_id.as_uuid(), id);
        assert_eq!(
            std::fs::read_dir(root.path().join("objects"))
                .unwrap()
                .count(),
            index + 1
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("staging"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[tokio::test]
async fn media_actual_rejected_png_is_terminal_and_symlinked_content_never_read() {
    let (f, report, revision, owner, root, actor) = setup().await;
    let id = Uuid::new_v4();
    let bad = b"not a PNG".to_vec();
    let raw = serde_json::to_vec(&media::UploadCreate {
        operation_id: media::Id::new(id).unwrap(),
        input_digest: media::Digest::of(&bad),
        input_bytes: bad.len() as u64,
        filename: "invalid.png".into(),
        capture: None,
        media: Default::default(),
        poster_artifact_id: None,
    })
    .unwrap();
    owner
        .reserve(&actor, report.report_id, &revision, &raw)
        .await
        .unwrap();
    owner.put(&actor, id, bad).await.unwrap();
    let rejected = owner.finalize(&actor, id).await.unwrap();
    assert_eq!(rejected.state, media::UploadState::Rejected);
    assert_eq!(rejected.rejection, Some(media::Rejection::InvalidPng));
    let pool = f.db.pool().await;
    assert!(
        sqlx::query(
            "UPDATE result_media_intents SET rejection='storage_failure' WHERE operation_id=$1"
        )
        .bind(id)
        .execute(&pool)
        .await
        .is_err()
    );
    assert_eq!(owner.cleanup(16).await.unwrap(), 1);
    let valid = retained(&owner, &actor, report.report_id, &revision).await;
    let path = root
        .path()
        .join("objects")
        .join(valid.to_string())
        .join("render.png");
    let outside = root.path().join("private.txt");
    std::fs::write(&outside, b"must never be served").unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    assert!(matches!(
        owner
            .content(&actor, report.report_id, &revision, valid, false)
            .await,
        Err(Error::Storage)
    ));
    assert_ne!(
        owner
            .list(&actor, report.report_id, &revision)
            .await
            .unwrap()
            .items[0]
            .availability
            .state,
        media::AvailabilityState::Available
    );
}

#[tokio::test]
async fn media_actual_replaced_lock_inode_cannot_start_second_owner() {
    let (f, _report, _revision, _owner, root, _actor) = setup().await;
    let pool = f.db.pool().await;
    let storage: Uuid = sqlx::query_scalar("SELECT storage_id FROM result_media_storage")
        .fetch_one(&pool)
        .await
        .unwrap();
    let marker = root.path().join("owner.lock");
    let old = root.path().join("original.lock");
    std::fs::rename(&marker, &old).unwrap();
    std::fs::copy(&old, &marker).unwrap();
    assert!(matches!(
        Owner::open(f.state.clone(), root.path(), storage).await,
        Err(Error::Storage)
    ));
    std::fs::remove_file(marker).unwrap();
    std::fs::rename(old, root.path().join("owner.lock")).unwrap();
}

#[tokio::test]
async fn media_actual_exact_put_replay_never_writes_or_renews_original_intent() {
    let (f, report, revision, owner, _root, actor) = setup().await;
    let id = Uuid::new_v4();
    owner
        .reserve(&actor, report.report_id, &revision, &request(id))
        .await
        .unwrap();
    let pool = f.db.pool().await;
    let before = intent_row(&pool, id).await;
    assert!(matches!(
        owner.put(&actor, id, vec![1, 2, 3]).await,
        Err(Error::Conflict)
    ));
    let unchanged = intent_row(&pool, id).await;
    assert_eq!(before, unchanged);
    owner.put(&actor, id, png()).await.unwrap();
    reject_replay_writes(&owner, "replayed PUT performed physical write");
    let received_before = intent_row(&pool, id).await;
    assert_eq!(
        owner.put(&actor, id, png()).await.unwrap().state,
        media::UploadState::Received
    );
    let received_after = intent_row(&pool, id).await;
    assert_eq!(received_before, received_after);
    owner.set_test_hook(None).unwrap();
    owner.finalize(&actor, id).await.unwrap();
    let retained_before = intent_row(&pool, id).await;
    reject_replay_writes(&owner, "retained replay performed physical write");
    assert_eq!(
        owner.put(&actor, id, png()).await.unwrap().state,
        media::UploadState::Retained
    );
    let retained_after = intent_row(&pool, id).await;
    assert_eq!(retained_before, retained_after);
}

#[tokio::test]
async fn media_actual_root_swap_after_receive_before_sql_commit_is_refused() {
    let (f, report, revision, owner, root, actor) = setup().await;
    let id = Uuid::new_v4();
    owner
        .reserve(&actor, report.report_id, &revision, &request(id))
        .await
        .unwrap();
    let gate = Gate::new();
    owner
        .set_test_hook(Some(gate.hook("received_before_commit")))
        .unwrap();
    let task = {
        let owner = owner.clone();
        let actor = actor.clone();
        tokio::spawn(async move { owner.put(&actor, id, png()).await })
    };
    gate.entered.notified().await;
    let moved = root.path().with_extension("moved-after-receive");
    std::fs::rename(root.path(), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, root.path()).unwrap();
    gate.release();
    assert!(matches!(task.await.unwrap(), Err(Error::Storage)));
    assert!(matches!(
        owner.status(&actor, id).await,
        Err(Error::Storage)
    ));
    let pool = f.db.pool().await;
    let state: String =
        sqlx::query_scalar("SELECT state FROM result_media_intents WHERE operation_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "reserved");
    std::fs::remove_file(root.path()).unwrap();
    std::fs::rename(moved, root.path()).unwrap();
    owner.set_test_hook(None).unwrap();
    f.time.store((NOW + 30_001) as i64, Ordering::SeqCst);
    assert_eq!(
        owner.put(&actor, id, png()).await.unwrap().state,
        media::UploadState::Received
    );
}

fn interrupt_once(owner: &Owner, point: &'static str) -> Arc<AtomicBool> {
    let fired = Arc::new(AtomicBool::new(false));
    let hit = fired.clone();
    owner
        .set_test_hook(Some(Arc::new(move |name| {
            if name == point && !hit.swap(true, Ordering::SeqCst) {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        })))
        .unwrap();
    fired
}

fn assert_durable_before_receipt(phases: &[String]) {
    let durable = phases
        .iter()
        .position(|p| p == "publication_durable")
        .expect("recovery must repeat fsync durability fence");
    let commit = phases
        .iter()
        .position(|p| p == "before_receipt_commit")
        .unwrap();
    assert!(durable < commit);
}

fn reject_replay_writes(owner: &Owner, message: &'static str) {
    owner
        .set_test_hook(Some(Arc::new(move |name| {
            if matches!(
                name,
                "input.png" | "render.png" | "preview.png" | "renamed" | "received_before_commit"
            ) {
                panic!("{message}");
            }
            Ok(())
        })))
        .unwrap();
}

async fn intent_row(pool: &sqlx::PgPool, id: Uuid) -> serde_json::Value {
    sqlx::query_scalar("SELECT to_jsonb(i) FROM result_media_intents i WHERE operation_id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
