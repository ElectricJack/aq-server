use aq_server::store::{NewNode, Role, Store, StoreError};
use aq_server::test_support::TestDb;
use aq_server::tokens::{new_enrollment_code, new_node_credential, verifier};
use chrono::{DateTime, Duration, TimeZone, Utc};
use std::sync::Arc;
use tokio::sync::Barrier;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
}

fn new_node(name: &str) -> NewNode {
    NewNode {
        name: name.into(),
        credential_hash: verifier(&new_node_credential()),
        worker_version: "0.1.0".into(),
        protocol_version: 1,
    }
}

#[tokio::test]
async fn reconnect_preserves_state() {
    let db = TestDb::create().await;
    let store = db.store().await;
    store
        .bootstrap_owner("o@x.com", "iss", "sub", t0())
        .await
        .unwrap()
        .unwrap();
    drop(store);

    let store2 = Store::connect_with(db.options()).await.unwrap();
    let user = store2
        .find_user_by_binding("iss", "sub")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(user.email, "o@x.com");
    assert_eq!(user.role, Role::Owner);
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let db = TestDb::create().await;
    drop(db.store().await);
    drop(Store::connect_with(db.options()).await.unwrap());
    let pool = db.pool().await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM schema_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, i64::from(aq_server::STATE_FORMAT_VERSION));
}

#[tokio::test]
async fn refuses_unknown_future_migration() {
    let db = TestDb::create().await;
    drop(db.store().await);
    let pool = db.pool().await;
    // One past the newest migration this binary knows.
    let future = aq_server::STATE_FORMAT_VERSION + 1;
    sqlx::query("INSERT INTO schema_migrations (version) VALUES ($1)")
        .bind(i64::from(future))
        .execute(&pool)
        .await
        .unwrap();
    let err = Store::connect_with(db.options()).await.err().unwrap();
    assert!(
        matches!(
            err,
            StoreError::IncompatibleState {
                found,
                supported
            } if found == future && supported == aq_server::STATE_FORMAT_VERSION
        ),
        "{err:?}"
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM schema_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, i64::from(aq_server::STATE_FORMAT_VERSION) + 1);
}

#[tokio::test]
async fn bootstrap_owner_only_once() {
    let db = TestDb::create().await;
    let store = db.store().await;
    assert!(
        store
            .bootstrap_owner("a@x.com", "i", "1", t0())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .bootstrap_owner("b@x.com", "i", "2", t0())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.list_users().await.unwrap().len(), 1);
}

#[tokio::test]
async fn email_unique_case_insensitive() {
    let db = TestDb::create().await;
    let store = db.store().await;
    store
        .invite_user("a@x.com", Role::Viewer, t0())
        .await
        .unwrap();
    let err = store
        .invite_user("A@x.com", Role::Viewer, t0())
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::EmailTaken), "{err:?}");
}

#[tokio::test]
async fn invited_user_binds_once() {
    let db = TestDb::create().await;
    let store = db.store().await;
    store
        .invite_user("a@x.com", Role::Viewer, t0())
        .await
        .unwrap();
    let u = store
        .bind_invited_user("A@x.com", "i", "1", t0())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u.role, Role::Viewer);
    assert_eq!(u.subject.as_deref(), Some("1"));
    assert!(
        store
            .bind_invited_user("a@x.com", "i", "2", t0())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .bind_invited_user("none@x.com", "i", "3", t0())
            .await
            .unwrap()
            .is_none()
    );
}

async fn owner_and_code(store: &Store, expires_in: Duration) -> (String, DateTime<Utc>) {
    let owner = store
        .bootstrap_owner("o@x.com", "i", "o", t0())
        .await
        .unwrap()
        .unwrap();
    let code = new_enrollment_code();
    let hash = verifier(&code);
    let expires = t0() + expires_in;
    store
        .create_enrollment(&hash, owner.id, t0(), expires)
        .await
        .unwrap();
    (hash, expires)
}

#[tokio::test]
async fn enrollment_code_single_use() {
    let db = TestDb::create().await;
    let store = db.store().await;
    let (hash, _) = owner_and_code(&store, Duration::minutes(10)).await;
    let node = store
        .redeem_enrollment(&hash, new_node("n1"), t0() + Duration::minutes(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node.name, "n1");
    assert!(
        store
            .redeem_enrollment(&hash, new_node("n2"), t0() + Duration::minutes(2))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn enrollment_code_expires() {
    let db = TestDb::create().await;
    let store = db.store().await;
    let (hash, expires) = owner_and_code(&store, Duration::minutes(10)).await;
    let r = store
        .redeem_enrollment(&hash, new_node("n"), expires + Duration::seconds(1))
        .await
        .unwrap();
    assert!(r.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_redeem_yields_one_node() {
    let db = TestDb::create().await;
    let store = db.store().await;
    let (hash, _) = owner_and_code(&store, Duration::minutes(10)).await;
    let now = t0() + Duration::minutes(1);
    let barrier = Arc::new(Barrier::new(2));
    let spawn_redeem = |name: &'static str| {
        let (store, hash, barrier) = (store.clone(), hash.clone(), barrier.clone());
        tokio::spawn(async move {
            barrier.wait().await;
            store.redeem_enrollment(&hash, new_node(name), now).await
        })
    };
    let (a, b) = (spawn_redeem("a"), spawn_redeem("b"));
    let (a, b) = (a.await.unwrap().unwrap(), b.await.unwrap().unwrap());
    assert_eq!(a.is_some() as u8 + b.is_some() as u8, 1);
    assert_eq!(store.list_nodes().await.unwrap().len(), 1);
}

#[tokio::test]
async fn node_lifecycle() {
    let db = TestDb::create().await;
    let store = db.store().await;
    let (hash, _) = owner_and_code(&store, Duration::minutes(10)).await;
    let nn = new_node("n");
    let cred = nn.credential_hash.clone();
    let node = store
        .redeem_enrollment(&hash, nn, t0())
        .await
        .unwrap()
        .unwrap();
    assert!(node.last_seen_at.is_none());

    let later = t0() + Duration::seconds(15);
    store
        .record_heartbeat(node.id, "0.2.0", 1, later)
        .await
        .unwrap();
    let found = store.find_node_by_credential(&cred).await.unwrap().unwrap();
    assert_eq!(found.worker_version, "0.2.0");
    assert_eq!(found.last_seen_at, Some(later));

    assert!(store.revoke_node(node.id, later).await.unwrap());
    assert!(!store.revoke_node(node.id, later).await.unwrap());
    let found = store.find_node_by_credential(&cred).await.unwrap().unwrap();
    assert_eq!(found.revoked_at, Some(later));
    assert!(
        store
            .find_node_by_credential("nope")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn refuses_newer_state_format() {
    let db = TestDb::create().await;
    drop(db.store().await);
    let pool = db.pool().await;
    sqlx::query("UPDATE meta SET value = $1 WHERE key = 'state_format_version'")
        .bind((aq_server::STATE_FORMAT_VERSION + 1).to_string())
        .execute(&pool)
        .await
        .unwrap();
    let err = Store::connect_with(db.options()).await.err().unwrap();
    assert!(
        matches!(
            err,
            StoreError::IncompatibleState {
                found,
                supported
            } if found == aq_server::STATE_FORMAT_VERSION + 1 && supported == aq_server::STATE_FORMAT_VERSION
        ),
        "{err:?}"
    );
}

#[test]
fn verifier_is_deterministic_and_hex64() {
    let v = verifier("secret");
    assert_eq!(v, verifier("secret"));
    assert_ne!(v, verifier("secret2"));
    assert_eq!(v.len(), 64);
    assert!(v.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
}

#[test]
fn tokens_have_prefixes_and_are_unique() {
    let (a, b) = (new_enrollment_code(), new_enrollment_code());
    assert!(a.starts_with("aqe_") && a != b);
    assert_eq!(a.len(), 4 + 27);
    let (c, d) = (new_node_credential(), new_node_credential());
    assert!(c.starts_with("aqn_") && c != d);
    assert_eq!(c.len(), 4 + 43);
}

#[tokio::test]
async fn exact_schema_two_upgrades_preserving_node_and_result_receipt() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    // Start with the actual prior SQL, not a current Store downgraded by metadata.
    let mut prior = pool.begin().await.unwrap();
    sqlx::raw_sql(include_str!("../../migrations/0001_foundation.sql"))
        .execute(&mut *prior)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../migrations/0002_execution.sql"))
        .execute(&mut *prior)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TIMESTAMPTZ NOT NULL DEFAULT now()); INSERT INTO schema_migrations(version) VALUES (1),(2)")
        .execute(&mut *prior)
        .await
        .unwrap();
    let node_id = uuid::Uuid::new_v4();
    let operation_id = uuid::Uuid::new_v4();
    let credential_hash = verifier("synthetic-prior-node-credential");
    let response = serde_json::json!({"report_id":operation_id,"disposition":"retained_stale","retention_release":false});
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,last_seen_at) VALUES($1,'prior-node',$2,'0.2.0',2,$3,$3)")
        .bind(node_id)
        .bind(&credential_hash)
        .bind(t0())
        .execute(&mut *prior)
        .await
        .unwrap();
    sqlx::query("INSERT INTO execution_receipts(node_id,operation_id,kind,payload_digest,response) VALUES($1,$2,'result',$3,$4)")
        .bind(node_id)
        .bind(operation_id)
        .bind("a".repeat(64))
        .bind(&response)
        .execute(&mut *prior)
        .await
        .unwrap();
    prior.commit().await.unwrap();
    let original_migrations: Vec<(i32, DateTime<Utc>)> =
        sqlx::query_as("SELECT version,applied_at FROM schema_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    let snapshot = async || {
        sqlx::query_scalar::<_, serde_json::Value>("SELECT jsonb_build_object('node',(SELECT to_jsonb(n) - 'machine_key' - 'slot' FROM nodes n WHERE id=$1),'receipt',(SELECT to_jsonb(r) FROM execution_receipts r WHERE node_id=$1 AND operation_id=$2))")
            .bind(node_id)
            .bind(operation_id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let before = snapshot().await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "2"
    );
    for _ in 0..2 {
        let store = Store::connect_with(db.options()).await.unwrap();
        let node = store
            .find_node_by_credential(&credential_hash)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(node.id, node_id);
        assert_eq!(node.worker_version, "0.2.0");
        assert_eq!(node.last_seen_at, Some(t0()));
        assert_eq!(snapshot().await, before);
        let migrations: Vec<(i32, DateTime<Utc>)> =
            sqlx::query_as("SELECT version,applied_at FROM schema_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(&migrations[..2], original_migrations.as_slice());
        assert_eq!(
            migrations.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
            (1..=aq_server::STATE_FORMAT_VERSION as i32).collect::<Vec<_>>()
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM meta WHERE key='state_format_version'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            aq_server::STATE_FORMAT_VERSION.to_string()
        );
        drop(store);
    }
}
