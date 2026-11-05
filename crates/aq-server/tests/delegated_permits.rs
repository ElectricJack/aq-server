use aq_admission_sandbox::{
    sandbox::{
        DelegatedPermit, GrantPhase, PermitBinding, PermitContext, PermitScope, RunnerOperation,
    },
    transport::WorkerIdentity,
};
use aq_server::delegated_permits::{CredentialKey, PermitRecord};
use uuid::Uuid;

mod common;

fn issued() -> DelegatedPermit {
    let permit_id = Uuid::from_u128(1);
    DelegatedPermit {
        schema_version: 1,
        permit_id,
        credential: CredentialKey::new([7; 32]).credential(permit_id).unwrap(),
        binding: PermitBinding {
            attempt_id: Uuid::from_u128(2),
            grant_generation: 3,
            sandbox_id: Uuid::from_u128(4),
            node: WorkerIdentity {
                node_id: Uuid::from_u128(5),
                boot_id: Uuid::from_u128(6),
            },
            channel_id: Uuid::from_u128(7),
        },
        scope: PermitScope::Preparation,
        issued_at_ms: 1_000,
        expires_at_ms: 121_000,
        hard_deadline_ms: 601_000,
    }
}

fn context(p: &DelegatedPermit) -> PermitContext {
    PermitContext {
        binding: p.binding.clone(),
        phase: GrantPhase::Preparing,
        grant_expires_at_ms: p.expires_at_ms,
        hard_deadline_ms: p.hard_deadline_ms,
    }
}

#[test]
fn permit_scope_refuses_foreign_attempt_and_expired_use() {
    let p = issued();
    let record = PermitRecord::from_issued(&p).unwrap();
    let current = context(&p);
    record
        .authorize(&current, RunnerOperation::Observe, 2_000)
        .unwrap();
    let mut foreign = current.clone();
    foreign.binding.attempt_id = Uuid::new_v4();
    assert!(
        record
            .authorize(&foreign, RunnerOperation::Observe, 2_000)
            .is_err()
    );
    foreign = current.clone();
    foreign.binding.node.boot_id = Uuid::new_v4();
    assert!(
        record
            .authorize(&foreign, RunnerOperation::Observe, 2_000)
            .is_err()
    );
    foreign = current.clone();
    foreign.binding.grant_generation += 1;
    assert!(
        record
            .authorize(&foreign, RunnerOperation::Observe, 2_000)
            .is_err()
    );
    for op in [
        RunnerOperation::Started,
        RunnerOperation::Renew,
        RunnerOperation::Terminal,
        RunnerOperation::ReportResult,
        RunnerOperation::AcceptOffer,
        RunnerOperation::RegisterIncarnation,
    ] {
        assert!(record.authorize(&current, op, 2_000).is_err(), "{op:?}");
    }
    assert!(
        record
            .authorize(&current, RunnerOperation::Observe, 999)
            .is_err()
    );
    assert!(
        record
            .authorize(&current, RunnerOperation::Observe, 121_000)
            .is_err()
    );
    let mut expired_grant = current.clone();
    expired_grant.grant_expires_at_ms = 1_500;
    assert!(
        record
            .authorize(&expired_grant, RunnerOperation::Observe, 2_000)
            .is_err()
    );
    let mut changed_deadline = current;
    changed_deadline.hard_deadline_ms += 1;
    assert!(
        record
            .authorize(&changed_deadline, RunnerOperation::Observe, 2_000)
            .is_err()
    );
}

#[test]
fn delegated_credentials_replay_without_persisting_token_and_fail_closed_on_key_change() {
    let p = issued();
    let record = PermitRecord::from_issued(&p).unwrap();
    let bytes = serde_json::to_vec(&record).unwrap();
    assert!(
        !String::from_utf8(bytes.clone())
            .unwrap()
            .contains(p.credential.expose())
    );
    assert!(!format!("{record:?}").contains(p.credential.expose()));
    let recovered: PermitRecord = serde_json::from_slice(&bytes).unwrap();
    let key = CredentialKey::new([7; 32]);
    assert_eq!(recovered.delivery(&key).unwrap(), p);
    assert!(recovered.delivery(&CredentialKey::new([8; 32])).is_err());
    assert!(recovered.verify_credential(p.credential.expose()).is_ok());
    assert!(recovered.verify_credential(&"x".repeat(256)).is_err());
    assert_ne!(key.credential(Uuid::from_u128(99)).unwrap(), p.credential);
    assert!(!format!("{key:?}").contains("7, 7"));
}

#[test]
fn delegated_revocation_and_corrupt_storage_cannot_recreate_authority() {
    let p = issued();
    let mut record = PermitRecord::from_issued(&p).unwrap();
    record.revoke();
    assert!(
        record
            .authorize(&context(&p), RunnerOperation::Observe, 2_000)
            .is_err()
    );
    assert!(record.delivery(&CredentialKey::new([7; 32])).is_err());
    let mut value = serde_json::to_value(PermitRecord::from_issued(&p).unwrap()).unwrap();
    value["expires_at_ms"] = serde_json::json!(p.hard_deadline_ms + 1);
    let corrupt: PermitRecord = serde_json::from_value(value).unwrap();
    assert!(
        corrupt
            .authorize(&context(&p), RunnerOperation::Observe, 2_000)
            .is_err()
    );
    assert!(corrupt.delivery(&CredentialKey::new([7; 32])).is_err());
}

#[tokio::test]
async fn delegated_storage_replays_rolls_back_and_survives_reconnect() {
    use aq_server::delegated_permits::storage;
    let f = common::profiled::fixture().await;
    let attempt = common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let key = CredentialKey::new([7; 32]);
    let mut p = issued();
    p.binding.attempt_id = attempt;
    p.binding.node = WorkerIdentity {
        node_id: f.node.node_id,
        boot_id: f.node.boot_id,
    };
    let operation = Uuid::new_v4();
    let digest = [42; 32];
    let mut tx = pool.begin().await.unwrap();
    storage::insert_locked(&mut tx, operation, digest, &p)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        storage::load_locked(&mut tx, p.permit_id)
            .await
            .unwrap()
            .is_none()
    );
    storage::insert_locked(&mut tx, operation, digest, &p)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    pool.close().await;

    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    let restored = storage::load_locked(&mut tx, p.permit_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.delivery(&key).unwrap(), p);
    let replay = storage::insert_locked(&mut tx, operation, digest, &p)
        .await
        .unwrap();
    assert_eq!(replay.delivery(&key).unwrap(), p);
    assert!(
        storage::insert_locked(&mut tx, operation, [43; 32], &p)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let raw: String = sqlx::query_scalar(
        "SELECT row_to_json(p)::text FROM delegated_permits p WHERE permit_id=$1",
    )
    .bind(p.permit_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!raw.contains(p.credential.expose()));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delegated_permits")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );

    f.state
        .store
        .revoke_node(f.node.node_id, (f.state.clock)())
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let revoked = storage::load_locked(&mut tx, p.permit_id)
        .await
        .unwrap()
        .unwrap();
    assert!(revoked.delivery(&key).is_err());
    assert!(
        revoked
            .authorize(&context(&p), RunnerOperation::Observe, 2_000)
            .is_err()
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn delegated_storage_cancellation_and_concurrent_replay() {
    use aq_server::delegated_permits::storage;
    let (f, start) = common::native_started::setup(true).await;
    let pool = f.db.pool().await;
    let key = CredentialKey::new([7; 32]);
    let mut p = issued();
    p.binding.attempt_id = start.binding.attempt_id;
    p.binding.node = WorkerIdentity {
        node_id: f.node.node_id,
        boot_id: f.node.boot_id,
    };
    p.scope = PermitScope::Execution;
    let operation = Uuid::new_v4();
    let issue = async {
        let mut tx = pool.begin().await.unwrap();
        let record = storage::insert_locked(&mut tx, operation, [42; 32], &p)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        record.delivery(&key).unwrap()
    };
    let replay = async {
        let mut candidate = p.clone();
        candidate.permit_id = Uuid::new_v4();
        candidate.credential = key.credential(candidate.permit_id).unwrap();
        let mut tx = pool.begin().await.unwrap();
        let record = storage::insert_locked(&mut tx, operation, [42; 32], &candidate)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        record.delivery(&key).unwrap()
    };
    let (first, second) = tokio::join!(issue, replay);
    assert_eq!(
        first, second,
        "only one concurrent candidate becomes durable"
    );
    let (status, body) = common::native_cancellation::cancel(
        &f.state,
        start.binding.attempt_id,
        Uuid::new_v4(),
        "cancel permit fixture",
    )
    .await;
    assert_eq!(status, 200, "{body:?}");
    let mut tx = pool.begin().await.unwrap();
    let revoked = storage::load_locked(&mut tx, first.permit_id)
        .await
        .unwrap()
        .unwrap();
    assert!(revoked.delivery(&key).is_err());
    tx.rollback().await.unwrap();
    assert!(
        sqlx::query("UPDATE delegated_permits SET revoked=FALSE WHERE permit_id=$1")
            .bind(first.permit_id)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM delegated_permits WHERE permit_id=$1")
            .bind(first.permit_id)
            .execute(&pool)
            .await
            .is_err()
    );
    p.permit_id = Uuid::new_v4();
    p.credential = key.credential(p.permit_id).unwrap();
    let mut tx = pool.begin().await.unwrap();
    let error = storage::insert_locked(&mut tx, Uuid::new_v4(), [44; 32], &p)
        .await
        .unwrap_err();
    assert!(
        matches!(error, storage::Error::Database(ref error)
        if error.as_database_error().and_then(|e| e.code()).as_deref() == Some("23514")),
        "cancelled attempt guard, not duplicate permit identity, must refuse: {error}"
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn delegated_storage_refuses_disabled_guards_and_truncation() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    assert!(
        sqlx::query("TRUNCATE delegated_permits")
            .execute(&pool)
            .await
            .is_err(),
        "replay/revocation evidence must be retained"
    );
    sqlx::query("ALTER TABLE delegated_permits DISABLE TRIGGER delegated_permit_guard")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        aq_server::store::Store::connect_with(f.db.options())
            .await
            .is_err(),
        "startup must refuse missing enforcement"
    );
}
