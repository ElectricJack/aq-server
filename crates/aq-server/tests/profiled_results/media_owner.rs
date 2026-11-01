//! Actual SQL result owner + managed files + real pinned PNG codec. Synthetic
//! launch fixture only; no provider, worker upload, HTTP or capture-success claim.
use super::*;
use aq_result_media as media;
use aq_server::{
    result_media::{Error, Owner},
    store::User,
};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Condvar, Mutex, atomic::AtomicBool};
pub(super) fn png() -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = !0u32;
        for b in &out[start..] {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    0xedb88320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
        }
        out.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]);
    let plain = [0u8, 10, 20, 30];
    let mut data = vec![0x78, 0x01, 1, 4, 0, 0xfb, 0xff];
    data.extend_from_slice(&plain);
    let (mut a, mut b) = (1u32, 0u32);
    for n in plain {
        a = (a + u32::from(n)) % 65521;
        b = (b + a) % 65521;
    }
    data.extend_from_slice(&((b << 16) | a).to_be_bytes());
    chunk(&mut out, b"IDAT", &data);
    chunk(&mut out, b"IEND", &[]);
    out
}
pub(super) async fn setup() -> (
    Fixture,
    ResultReport,
    String,
    Owner,
    tempfile::TempDir,
    User,
) {
    let (f, _, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let recorded = results::record(&f.state, &auth, &report).await.unwrap();
    let actor = f
        .state
        .store
        .find_user_by_binding("dev", "owner@localhost")
        .await
        .unwrap()
        .unwrap();
    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let owner = Owner::open(f.state.clone(), root.path(), Uuid::new_v4())
        .await
        .unwrap();
    (
        f,
        report,
        recorded.report_digest.as_str().into(),
        owner,
        root,
        actor,
    )
}
pub(super) fn request(id: Uuid) -> Vec<u8> {
    let bytes = png();
    serde_json::to_vec(&media::UploadCreate {
        operation_id: media::Id::new(id).unwrap(),
        input_digest: media::Digest::of(&bytes),
        input_bytes: bytes.len() as u64,
        filename: "human-supplied.png".into(),
        capture: None,
    })
    .unwrap()
}
pub(super) async fn retained(owner: &Owner, actor: &User, report: Uuid, revision: &str) -> Uuid {
    let id = Uuid::new_v4();
    owner
        .reserve(actor, report, revision, &request(id))
        .await
        .unwrap();
    owner.put(actor, id, png()).await.unwrap();
    let status = owner.finalize(actor, id).await.unwrap();
    assert_eq!(status.state, media::UploadState::Retained);
    id
}
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
pub(super) struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
    pub(super) entered: tokio::sync::Notify,
    once: AtomicBool,
}
impl Gate {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            open: Mutex::new(false),
            cv: Condvar::new(),
            entered: tokio::sync::Notify::new(),
            once: AtomicBool::new(false),
        })
    }
    pub(super) fn hook(self: &Arc<Self>, point: &'static str) -> aq_server::result_media::TestHook {
        let gate = self.clone();
        Arc::new(move |name| {
            if name == point && !gate.once.swap(true, Ordering::SeqCst) {
                gate.entered.notify_one();
                let mut open = gate.open.lock().unwrap();
                while !*open {
                    open = gate.cv.wait(open).unwrap();
                }
            }
            Ok(())
        })
    }
    pub(super) fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}
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
        let phases = crossed.lock().unwrap();
        let durable = phases
            .iter()
            .position(|p| p == "publication_durable")
            .expect("recovery must repeat fsync durability fence");
        let commit = phases
            .iter()
            .position(|p| p == "before_receipt_commit")
            .unwrap();
        assert!(durable < commit);
        drop(phases);
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
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(i) FROM result_media_intents i WHERE operation_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(matches!(
        owner.put(&actor, id, vec![1, 2, 3]).await,
        Err(Error::Conflict)
    ));
    let unchanged: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(i) FROM result_media_intents i WHERE operation_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, unchanged);
    owner.put(&actor, id, png()).await.unwrap();
    owner
        .set_test_hook(Some(Arc::new(|name| {
            if matches!(
                name,
                "input.png" | "render.png" | "preview.png" | "renamed" | "received_before_commit"
            ) {
                panic!("replayed PUT performed physical write")
            } else {
                Ok(())
            }
        })))
        .unwrap();
    let received_before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(i) FROM result_media_intents i WHERE operation_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        owner.put(&actor, id, png()).await.unwrap().state,
        media::UploadState::Received
    );
    let received_after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(i) FROM result_media_intents i WHERE operation_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(received_before, received_after);
    owner.set_test_hook(None).unwrap();
    owner.finalize(&actor, id).await.unwrap();
    let retained_before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(i) FROM result_media_intents i WHERE operation_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    owner
        .set_test_hook(Some(Arc::new(|name| {
            if matches!(
                name,
                "input.png" | "render.png" | "preview.png" | "renamed" | "received_before_commit"
            ) {
                panic!("retained replay performed physical write")
            } else {
                Ok(())
            }
        })))
        .unwrap();
    assert_eq!(
        owner.put(&actor, id, png()).await.unwrap().state,
        media::UploadState::Retained
    );
    let retained_after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(i) FROM result_media_intents i WHERE operation_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
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
                    owner.finalize(&actor, id).await,
                    Err(Error::Storage)
                ));
                owner.set_test_hook(None).unwrap();
            }
            ids.push(id);
        }
        let before:serde_json::Value=sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(i)-ARRAY['state','transition_at_ms','reserved_bytes','retained_bytes','lease_id','lease_expires_at_ms','cleanup_state','cleanup_generation','cleanup_completed_at_ms'] ORDER BY operation_id) FROM result_media_intents i WHERE operation_id=ANY($1)").bind(&ids).fetch_one(&pool).await.unwrap();
        at += media::MAX_UPLOAD_TTL_MS + 1;
        f.time.store(at as i64, Ordering::SeqCst);
        assert_eq!(owner.cleanup(16).await.unwrap(), count);
        let after:serde_json::Value=sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(i)-ARRAY['state','transition_at_ms','reserved_bytes','retained_bytes','lease_id','lease_expires_at_ms','cleanup_state','cleanup_generation','cleanup_completed_at_ms'] ORDER BY operation_id) FROM result_media_intents i WHERE operation_id=ANY($1)").bind(&ids).fetch_one(&pool).await.unwrap();
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
