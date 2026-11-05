//! Disposable prefix upgrades and actual SQL drift/source-corruption refusal.
use super::{MIGRATIONS, StoreError, migrate_known};
use crate::test_support::TestDb;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

async fn database(version: u32) -> (TestDb, PgPool) {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_known(&pool, &MIGRATIONS[..version as usize], version)
        .await
        .unwrap();
    (db, pool)
}
async fn rows(pool: &PgPool, table: &str) -> Value {
    sqlx::query_scalar(&format!("SELECT COALESCE(jsonb_agg(row ORDER BY row::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) AS row FROM {table} t) s"))
        .fetch_one(pool).await.unwrap()
}
fn digest(value: &Value) -> String {
    hex::encode(Sha256::digest(aq_agent_api::canonical(value).unwrap()))
}
// SQL-seeded exact frozen API07 envelope/receipt, not browser/delivery evidence.
async fn seed_original(pool: &PgPool) -> (Uuid, Uuid) {
    let conversation: Uuid = sqlx::query_scalar(
        "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let user = Uuid::new_v4();
    let message = Uuid::new_v4();
    let operation = Uuid::new_v4();
    let sender = json!({"kind":"owner","user_id":user});
    let recipient = json!({"kind":"global_supervisor","conversation_id":conversation});
    let key = digest(&recipient);
    let sender_key = digest(&sender);
    let body = "Original retained global body ☃";
    let request = json!({"operation_id":operation,"message_id":message,"expected_conversation_revision":"1","body":body,"context":[],"attachments":[]});
    let payload = digest(
        &json!({"kind":"global_supervisor_send","conversation_id":conversation,"request":request,"sender":sender}),
    );
    let receipt = json!({"contract":"human-supervisor-conversation-1","conversation":{"id":conversation,"revision":"1","label":"Global supervisor"},"runtime":{"state":"unavailable","binding":null,"reason":"central_runtime_not_connected"},"operation_id":operation,"message_id":message,"actor_id":user,"request_digest":format!("sha256:{}",digest(&request)),"body_digest":format!("sha256:{}",hex::encode(Sha256::digest(body.as_bytes()))),"state":"queued","cursor":"1","reason":null,"host_delivery":"not_attempted","agent_receipt":null});
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at,bound_at) VALUES($1,'retained@localhost','owner','dev','retained@localhost',now(),now())").bind(user).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO mailbox_heads(mailbox_key,next_cursor) VALUES($1,2)")
        .bind(&key)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mailbox_messages(message_id,mailbox_key,cursor,thread_id,sender,recipient,payload_digest,body,created_at_ms) VALUES($1,$2,1,$3,$4,$5,$6,$7,1000)")
        .bind(message).bind(key).bind(conversation).bind(sender).bind(recipient).bind(&payload).bind(body).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO mailbox_operations(sender_key,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)").bind(sender_key).bind(operation).bind(payload).bind(receipt).execute(pool).await.unwrap();
    sqlx::query(
        "UPDATE global_supervisor_conversation SET retained_operations=1,retained_bytes=$1",
    )
    .bind(body.len() as i64 + 4096)
    .execute(pool)
    .await
    .unwrap();
    (message, operation)
}

#[tokio::test]
async fn fresh37_actual36_upgrade_preserves_global_mail_media_and_refuses_old36_reader() {
    let (_db, pool) = database(36).await;
    seed_original(&pool).await;
    sqlx::query("INSERT INTO result_media_storage(storage_id,root_fingerprint,created_at_ms) VALUES($1,repeat('c',64),1000)").bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    let tables = [
        "global_supervisor_conversation",
        "mailbox_heads",
        "mailbox_messages",
        "mailbox_operations",
        "result_media_storage",
        "result_media_budgets",
        "result_media_intents",
    ];
    let mut before = Vec::new();
    for table in tables {
        before.push(rows(&pool, table).await);
    }
    if let Err(error) = migrate_known(&pool, &MIGRATIONS[..37], 37).await {
        panic!("fresh36→37 qualification failed: {error:?}");
    }
    for (table, old) in tables.into_iter().zip(&before) {
        assert_eq!(rows(&pool, table).await, *old, "{table}");
    }
    let ledger = rows(&pool, "schema_migrations").await;
    let meta = rows(&pool, "meta").await;
    assert!(matches!(
        migrate_known(&pool, &MIGRATIONS[..36], 36).await,
        Err(StoreError::IncompatibleState {
            found: 37,
            supported: 36
        })
    ));
    assert_eq!(rows(&pool, "schema_migrations").await, ledger);
    assert_eq!(rows(&pool, "meta").await, meta);
    for (table, old) in tables.into_iter().zip(before) {
        assert_eq!(rows(&pool, table).await, old, "{table}");
    }
    migrate_known(&pool, &MIGRATIONS[..37], 37).await.unwrap();
    let (_fresh, fresh) = database(37).await;
    assert_eq!(rows(&fresh, "central_supervisor_actors").await, json!([]));
    assert_eq!(
        rows(&fresh, "central_supervisor_operations").await,
        json!([])
    );
}

async fn drift(sql: &str) {
    let (_db, pool) = database(37).await;
    sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    let ledger = rows(&pool, "schema_migrations").await;
    let meta = rows(&pool, "meta").await;
    let before = rows(&pool, "global_supervisor_conversation").await;
    assert!(
        matches!(migrate_known(&pool,&MIGRATIONS[..37],37).await,Err(StoreError::Corrupt(message)) if message.contains("central reservation")),
        "{sql}"
    );
    assert_eq!(rows(&pool, "schema_migrations").await, ledger);
    assert_eq!(rows(&pool, "meta").await, meta);
    assert_eq!(rows(&pool, "global_supervisor_conversation").await, before);
}
#[tokio::test]
async fn missing_disabled_wrong_or_non_deferred37_guards_refuse_without_repair() {
    for (table, guard) in [
        ("mailbox_messages", "central_global_mail_row_guard"),
        (
            "mailbox_operations",
            "central_global_receipt_truncate_guard",
        ),
        ("central_supervisor_actors", "central_actor_row_guard"),
        (
            "central_supervisor_reservations",
            "central_reservation_consistency",
        ),
        (
            "central_supervisor_mail_members",
            "central_member_consistency",
        ),
        (
            "central_supervisor_operations",
            "central_operation_consistency",
        ),
    ] {
        drift(&format!("DROP TRIGGER {guard} ON {table}")).await;
        drift(&format!("ALTER TABLE {table} DISABLE TRIGGER {guard}")).await;
    }
    drift("CREATE OR REPLACE FUNCTION guard_central_global_mail() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$").await;
    drift("ALTER FUNCTION central_global_mail_source_valid(uuid) IMMUTABLE").await;
    drift("DROP TRIGGER central_member_consistency ON central_supervisor_mail_members; CREATE TRIGGER central_member_consistency AFTER INSERT ON central_supervisor_mail_members FOR EACH ROW EXECUTE FUNCTION check_central_reservation_consistency()").await;
    drift("DROP TRIGGER central_actor_row_guard ON central_supervisor_actors; CREATE TRIGGER central_actor_row_guard BEFORE UPDATE ON central_supervisor_actors FOR EACH ROW EXECUTE FUNCTION guard_central_reservation_actor()").await;
}

#[tokio::test]
async fn same_named37_checks_keys_indexes_fks_and_nullability_cannot_bypass_startup() {
    for sql in [
        "ALTER TABLE central_supervisor_operations DROP CONSTRAINT central_supervisor_operations_pkey CASCADE",
        "ALTER TABLE central_supervisor_mail_members DROP CONSTRAINT central_supervisor_mail_members_pkey; ALTER TABLE central_supervisor_mail_members ADD CONSTRAINT central_supervisor_mail_members_pkey PRIMARY KEY(reservation_id,ordinal,message_id)",
        "ALTER TABLE central_supervisor_actors ALTER COLUMN approved_identity DROP NOT NULL",
        "DO $$ DECLARE t record; BEGIN FOR t IN SELECT tgname FROM pg_trigger WHERE tgrelid='central_supervisor_mail_members'::regclass AND tgisinternal LOOP EXECUTE format('ALTER TABLE central_supervisor_mail_members DISABLE TRIGGER %I',t.tgname); END LOOP; END $$",
        "ALTER TABLE central_supervisor_operations ADD CONSTRAINT unexpected37_write_refusal CHECK(FALSE)",
        "ALTER TABLE central_supervisor_mail_members DROP CONSTRAINT central_supervisor_mail_members_message_id_fkey; ALTER TABLE central_supervisor_mail_members ADD CONSTRAINT central_supervisor_mail_members_message_id_fkey FOREIGN KEY(message_id) REFERENCES mailbox_messages(message_id) ON DELETE CASCADE",
        "ALTER TABLE central_supervisor_reservations DROP CONSTRAINT central_reservation_cancel_operation; ALTER TABLE central_supervisor_reservations ADD CONSTRAINT central_reservation_cancel_operation FOREIGN KEY(actor_id,cancel_operation_id) REFERENCES central_supervisor_operations(actor_id,operation_id)",
        "DROP INDEX central_supervisor_member_cursor; CREATE INDEX central_supervisor_member_cursor ON central_supervisor_mail_members(actor_id,cursor,reservation_id)",
        "DROP INDEX central_supervisor_reservation_history; CREATE INDEX central_supervisor_reservation_history ON central_supervisor_reservations(actor_id,admission_actor_revision)",
        "DROP INDEX central_supervisor_one_active; CREATE UNIQUE INDEX central_supervisor_one_active ON central_supervisor_reservations(actor_id) WHERE state='cancelled_before_plan'",
        "DROP INDEX mailbox_global_queued_original; CREATE INDEX mailbox_global_queued_original ON mailbox_operations((response->>'message_id')) WHERE response->>'state'='queued'",
        "ALTER TABLE central_supervisor_actors DROP CONSTRAINT central_supervisor_actors_check; ALTER TABLE central_supervisor_actors ADD CONSTRAINT central_supervisor_actors_check CHECK(TRUE)",
        "ALTER TABLE central_supervisor_actors DROP CONSTRAINT central_supervisor_actors_check3; ALTER TABLE central_supervisor_actors ADD CONSTRAINT central_supervisor_actors_check3 CHECK(TRUE)",
        "ALTER TABLE central_supervisor_actors DROP CONSTRAINT central_supervisor_actors_check4; ALTER TABLE central_supervisor_actors ADD CONSTRAINT central_supervisor_actors_check4 CHECK(TRUE)",
        "ALTER TABLE central_supervisor_reservations DROP CONSTRAINT central_supervisor_reservations_check2; ALTER TABLE central_supervisor_reservations ADD CONSTRAINT central_supervisor_reservations_check2 CHECK(TRUE)",
        "ALTER TABLE central_supervisor_operations DROP CONSTRAINT central_supervisor_operations_check2; ALTER TABLE central_supervisor_operations ADD CONSTRAINT central_supervisor_operations_check2 CHECK(TRUE)",
        "ALTER TABLE central_supervisor_operations DROP CONSTRAINT central_supervisor_operations_check3; ALTER TABLE central_supervisor_operations ADD CONSTRAINT central_supervisor_operations_check3 CHECK(TRUE)",
        "ALTER TABLE central_supervisor_operations DROP CONSTRAINT central_supervisor_operations_check1; ALTER TABLE central_supervisor_operations ADD CONSTRAINT central_supervisor_operations_check1 CHECK(TRUE)",
    ] {
        drift(sql).await;
    }
}

#[tokio::test]
async fn retained_owner_reserve_and_cancel_proofs_are_revalidated_after_guards_reenabled() {
    use crate::agent_access::supervisor_conversation::reservations::{
        PreviewRequest, ReservationOwner,
    };
    use crate::{AppState, config::Config, identity::verifier_from_config};
    use chrono::{TimeZone, Utc};
    use std::sync::Arc;
    for damage in [
        "ALTER TABLE central_supervisor_actors DISABLE TRIGGER USER; UPDATE central_supervisor_actors SET retained_bytes=retained_bytes+1; ALTER TABLE central_supervisor_actors ENABLE TRIGGER USER",
        "ALTER TABLE central_supervisor_operations DISABLE TRIGGER USER; UPDATE central_supervisor_operations SET result_bytes=replace(result_bytes,result_bytes::jsonb#>>'{reservation,batch_digest}',repeat('0',64)) WHERE kind='reserve'; ALTER TABLE central_supervisor_operations ENABLE TRIGGER USER",
        "ALTER TABLE central_supervisor_mail_members DISABLE TRIGGER USER; UPDATE central_supervisor_mail_members SET payload_digest=repeat('0',64); ALTER TABLE central_supervisor_mail_members ENABLE TRIGGER USER",
        "ALTER TABLE central_supervisor_reservations DISABLE TRIGGER USER; UPDATE central_supervisor_reservations SET cancel_actor_revision=cancel_actor_revision+1; ALTER TABLE central_supervisor_reservations ENABLE TRIGGER USER",
        "ALTER TABLE central_supervisor_mail_members DISABLE TRIGGER ALL; INSERT INTO central_supervisor_mail_members(actor_id,reservation_id,ordinal,message_id,mailbox_key,cursor,payload_digest) SELECT actor_id,gen_random_uuid(),ordinal,message_id,mailbox_key,cursor,payload_digest FROM central_supervisor_mail_members; ALTER TABLE central_supervisor_mail_members ENABLE TRIGGER ALL",
        "ALTER TABLE central_supervisor_operations DISABLE TRIGGER ALL; WITH original AS (SELECT *,gen_random_uuid() AS orphan_actor FROM central_supervisor_operations WHERE kind='configure' LIMIT 1), changed_request AS (SELECT *,replace(request_bytes,actor_id::text,orphan_actor::text) AS new_request FROM original), changed_digest AS (SELECT *,encode(sha256(convert_to(new_request,'UTF8')),'hex') AS new_digest FROM changed_request) INSERT INTO central_supervisor_operations(actor_id,operation_id,kind,actor_revision,initiating_user_id,request_bytes,request_digest,result_bytes,charged_bytes) SELECT orphan_actor,operation_id,kind,actor_revision,initiating_user_id,new_request,new_digest,replace(replace(result_bytes,actor_id::text,orphan_actor::text),request_digest,new_digest),charged_bytes FROM changed_digest; ALTER TABLE central_supervisor_operations ENABLE TRIGGER ALL",
    ] {
        let (db, pool) = database(37).await;
        seed_original(&pool).await;
        let store = db.store_at_version(37).await;
        let user = store
            .find_user_by_binding("dev", "retained@localhost")
            .await
            .unwrap()
            .unwrap();
        let config:Config=toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='retained@localhost'\n[bootstrap]\nowner_email='retained@localhost'\nenabled=false\n").unwrap();
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
        let state = AppState {
            store,
            identity: verifier_from_config(&config.auth, reqwest::Client::new()),
            config: Arc::new(config),
            clock: Arc::new(move || now),
            agent_access: None,
            source_retention: None,
        };
        let owner = ReservationOwner::new(&state);
        let actor = Uuid::new_v4();
        let conversation: Uuid = sqlx::query_scalar(
            "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        owner.configure_actor(&user,&serde_json::to_vec(&json!({"operation_id":Uuid::new_v4(),"actor_id":actor,"expected_actor_revision":0,"policy":{"schema_version":1,"kind":"global_supervisor_reservation","actor_id":actor,"conversation_id":conversation,"enabled":true}})).unwrap()).await.unwrap();
        let preview = owner
            .preview_batch(
                &user,
                PreviewRequest {
                    actor_id: actor,
                    expected_actor_revision: 1,
                    scan_after: 0,
                    through_cursor: None,
                },
            )
            .await
            .unwrap();
        let reservation = Uuid::new_v4();
        owner.reserve_batch(&user,&serde_json::to_vec(&json!({"operation_id":Uuid::new_v4(),"reservation_id":reservation,"actor_id":actor,"expected_actor_revision":preview.actor_revision,"expected_policy_revision":preview.policy_revision,"scan_after":preview.scan_after,"through_cursor":preview.through_cursor,"selection_digest":preview.selection_digest})).unwrap()).await.unwrap();
        migrate_known(&pool, &MIGRATIONS[..37], 37).await.unwrap();
        owner.cancel_reservation(&user,&serde_json::to_vec(&json!({"operation_id":Uuid::new_v4(),"actor_id":actor,"reservation_id":reservation,"expected_reservation_revision":1})).unwrap()).await.unwrap();
        migrate_known(&pool, &MIGRATIONS[..37], 37).await.unwrap();
        sqlx::raw_sql(damage).execute(&pool).await.unwrap();
        let tables = [
            "central_supervisor_actors",
            "central_supervisor_reservations",
            "central_supervisor_mail_members",
            "central_supervisor_operations",
            "schema_migrations",
            "meta",
        ];
        let mut before = Vec::new();
        for table in tables {
            before.push(rows(&pool, table).await);
        }
        assert!(
            matches!(
                migrate_known(&pool, &MIGRATIONS[..37], 37).await,
                Err(StoreError::Corrupt(_))
            ),
            "{damage}"
        );
        for (table, previous) in tables.into_iter().zip(before) {
            assert_eq!(rows(&pool, table).await, previous, "{table}");
        }
    }
}

#[tokio::test]
async fn corrupt_pre37_global_body_or_orphan_original_proof_refuses_atomic_upgrade() {
    for mode in 0..3 {
        let (_db, pool) = database(36).await;
        let (message, _) = seed_original(&pool).await;
        if mode == 1 {
            sqlx::query("DELETE FROM mailbox_messages WHERE message_id=$1")
                .bind(message)
                .execute(&pool)
                .await
                .unwrap();
        } else if mode == 2 {
            sqlx::query("INSERT INTO mailbox_operations(sender_key,operation_id,payload_digest,response) SELECT sender_key,gen_random_uuid(),payload_digest,jsonb_set(response,'{body_digest}',to_jsonb('sha256:'||repeat('0',64))) FROM mailbox_operations")
                .execute(&pool).await.unwrap();
        } else {
            sqlx::query("UPDATE mailbox_messages SET body='Changed retained global body ☃' WHERE message_id=$1").bind(message).execute(&pool).await.unwrap();
        }
        let ledger = rows(&pool, "schema_migrations").await;
        let meta = rows(&pool, "meta").await;
        let messages = rows(&pool, "mailbox_messages").await;
        let operations = rows(&pool, "mailbox_operations").await;
        assert!(migrate_known(&pool, &MIGRATIONS[..37], 37).await.is_err());
        assert_eq!(rows(&pool, "schema_migrations").await, ledger);
        assert_eq!(rows(&pool, "meta").await, meta);
        assert_eq!(rows(&pool, "mailbox_messages").await, messages);
        assert_eq!(rows(&pool, "mailbox_operations").await, operations);
        let exists: bool =
            sqlx::query_scalar("SELECT to_regclass('central_supervisor_actors') IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!exists, "failed upgrade must not publish partial37storage");
    }
}

#[tokio::test]
async fn post37_source_mutation_or_head_rewind_is_refused_without_repair() {
    for head in [false, true] {
        let (_db, pool) = database(36).await;
        seed_original(&pool).await;
        migrate_known(&pool, &MIGRATIONS[..37], 37).await.unwrap();
        if head {
            sqlx::query("UPDATE mailbox_heads SET next_cursor=1")
                .execute(&pool)
                .await
                .unwrap();
        } else {
            sqlx::raw_sql("ALTER TABLE mailbox_messages DISABLE TRIGGER central_global_mail_row_guard; UPDATE mailbox_messages SET body='changed'; ALTER TABLE mailbox_messages ENABLE TRIGGER central_global_mail_row_guard").execute(&pool).await.unwrap();
        }
        let messages = rows(&pool, "mailbox_messages").await;
        let ledger = rows(&pool, "schema_migrations").await;
        let meta = rows(&pool, "meta").await;
        assert!(matches!(
            migrate_known(&pool, &MIGRATIONS[..37], 37).await,
            Err(StoreError::Corrupt(_))
        ));
        assert_eq!(rows(&pool, "mailbox_messages").await, messages);
        assert_eq!(rows(&pool, "schema_migrations").await, ledger);
        assert_eq!(rows(&pool, "meta").await, meta);
    }
}
