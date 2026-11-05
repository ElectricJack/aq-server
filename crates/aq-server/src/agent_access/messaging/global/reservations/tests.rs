//! Disposable persisted reservation qualification; no host/provider/runtime exists.
use super::*;
use crate::{config::Config, identity::verifier_from_config, test_support::TestDb};
use axum::body::Bytes;
use chrono::{TimeZone, Utc};
use std::sync::Arc;
struct Fixture {
    db: TestDb,
    state: AppState,
    user: User,
    owner: Arc<ReservationOwner>,
    actor: Uuid,
    conversation: Uuid,
}
impl Fixture {
    async fn new(register: bool) -> Self {
        let db = TestDb::create().await;
        let store = db.store_at_version(37).await;
        let version: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(version, "37", "owner fixtures must remain on registered37");
        let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
        let user = store
            .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
            .await
            .unwrap()
            .unwrap();
        let config:Config=toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n").unwrap();
        let state = AppState {
            store,
            identity: verifier_from_config(&config.auth, reqwest::Client::new()),
            config: Arc::new(config),
            clock: Arc::new(move || now),
            agent_access: None,
            source_retention: None,
        };
        let conversation = sqlx::query_scalar(
            "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
        )
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
        let owner = Arc::new(ReservationOwner::new(&state));
        let f = Self {
            db,
            state,
            user,
            owner,
            actor: Uuid::new_v4(),
            conversation,
        };
        if register {
            let receipt = f.configure(true, 0).await.unwrap();
            assert_eq!(receipt.disposition, Disposition::Configured);
        }
        f
    }
    fn policy(&self, enabled: bool) -> LocalPolicy {
        LocalPolicy {
            schema_version: 1,
            kind: PolicyKind::GlobalSupervisorReservation,
            actor_id: self.actor,
            conversation_id: self.conversation,
            enabled,
        }
    }
    async fn configure(&self, enabled: bool, revision: u64) -> Result<Receipt> {
        self.owner
            .configure_actor(
                &self.user,
                &serde_json::to_vec(&Configure {
                    operation_id: Uuid::new_v4(),
                    actor_id: self.actor,
                    expected_actor_revision: revision,
                    policy: self.policy(enabled),
                })
                .unwrap(),
            )
            .await
    }
    async fn seed(&self, body: &str) -> Uuid {
        let message = Uuid::new_v4();
        let raw = json!({"operation_id":Uuid::new_v4(),"message_id":message,"expected_conversation_revision":"1","body":body,"context":[],"attachments":[]});
        // This qualification stays on registered37. Exercise the actual mailbox
        // writer, while the HTTP action wrapper requires the schema42 journal.
        let response = super::super::writes::enqueue_historical_fixture(
            &self.state,
            &self.user,
            self.conversation,
            Bytes::from(raw.to_string()),
        )
        .await
        .unwrap();
        assert!(response.status().is_success());
        let (version, dashboard_absent): (String, bool) = sqlx::query_as(
            "SELECT value, to_regclass('dashboard_operations') IS NULL FROM meta WHERE key='state_format_version'",
        )
        .fetch_one(&self.state.store.pool)
        .await
        .unwrap();
        assert_eq!(version, "37", "seeding must preserve registered37");
        assert!(
            dashboard_absent,
            "registered37 has no dashboard action journal"
        );
        message
    }
    async fn preview(&self, after: u64) -> Preview {
        let a = self
            .owner
            .inspect_actor(&self.user, self.actor)
            .await
            .unwrap();
        self.owner
            .preview_batch(
                &self.user,
                PreviewRequest {
                    actor_id: self.actor,
                    expected_actor_revision: a.revision,
                    scan_after: after,
                    through_cursor: None,
                },
            )
            .await
            .unwrap()
    }
    fn reserve_request(&self, p: &Preview) -> Vec<u8> {
        serde_json::to_vec(&Reserve {
            operation_id: Uuid::new_v4(),
            reservation_id: Uuid::new_v4(),
            actor_id: self.actor,
            expected_actor_revision: p.actor_revision,
            expected_policy_revision: p.policy_revision,
            scan_after: p.scan_after,
            through_cursor: p.through_cursor,
            selection_digest: p.selection_digest.clone(),
        })
        .unwrap()
    }
    async fn reserve(&self) -> (Vec<u8>, Receipt) {
        let raw = self.reserve_request(&self.preview(0).await);
        let receipt = self.owner.reserve_batch(&self.user, &raw).await.unwrap();
        assert_eq!(receipt.disposition, Disposition::Reserved);
        (raw, receipt)
    }
    async fn cancel(&self, reservation: &ReservationView) -> Receipt {
        let raw = serde_json::to_vec(&Cancel {
            operation_id: Uuid::new_v4(),
            actor_id: self.actor,
            reservation_id: reservation.id,
            expected_reservation_revision: reservation.revision,
        })
        .unwrap();
        self.owner
            .cancel_reservation(&self.user, &raw)
            .await
            .unwrap()
    }
    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&self.state.store.pool)
            .await
            .unwrap()
    }
}
#[tokio::test]
async fn actual_mail_reservation_recovers_replays_and_cancels_without_runtime() {
    let f = Fixture::new(true).await;
    let empty = f.preview(0).await;
    assert_eq!(empty.status, PreviewStatus::NoEligibleMail);
    assert_eq!(empty.coverage, Coverage::Complete);
    assert!(empty.batch.is_none());
    sqlx::query("INSERT INTO mailbox_heads(mailbox_key,next_cursor) VALUES($1,1)")
        .bind(key(f.conversation).unwrap())
        .execute(&f.state.store.pool)
        .await
        .unwrap();
    assert_eq!(
        f.preview(0).await,
        empty,
        "an empty head and absent empty head are the same logical mailbox"
    );
    let message = f.seed("Hello ☃\nKeep actual mailbox truth.").await;
    f.seed("Second message.").await;
    let before: Value =
        sqlx::query_scalar("SELECT to_jsonb(g) FROM global_supervisor_conversation g")
            .fetch_one(&f.state.store.pool)
            .await
            .unwrap();
    let (raw, receipt) = f.reserve().await;
    let saved = receipt.reservation.clone().unwrap();
    assert_eq!(saved.batch.messages[0].message_id, message);
    let wire = serde_json::to_value(&saved).unwrap();
    assert!(wire["session"].is_null());
    assert!(wire["prepared_plan"].is_null());
    assert_eq!(wire["runtime"], "unavailable");
    assert!(
        !serde_json::to_string(&saved)
            .unwrap()
            .contains("Keep actual")
    );
    let restarted = ReservationOwner::new(&f.state);
    let lookup = restarted
        .lookup_operation(&f.user, f.actor, receipt.operation_id)
        .await
        .unwrap();
    assert_eq!(lookup.receipt, receipt);
    assert_eq!(lookup.current_reservation, Some(saved.clone()));
    assert_eq!(
        restarted
            .reserve_batch(
                &f.user,
                &serde_json::to_vec_pretty(&serde_json::from_slice::<Value>(&raw).unwrap())
                    .unwrap()
            )
            .await
            .unwrap(),
        receipt
    );
    let mut changed: Value = serde_json::from_slice(&raw).unwrap();
    changed["reservation_id"] = json!(Uuid::new_v4());
    assert!(matches!(
        restarted
            .reserve_batch(&f.user, &serde_json::to_vec(&changed).unwrap())
            .await,
        Err(AppError::Conflict(_))
    ));
    let disabled = f.configure(false, receipt.actor_revision).await.unwrap();
    assert_eq!(disabled.disposition, Disposition::Configured);
    assert_eq!(f.preview(0).await.status, PreviewStatus::Disabled);
    assert_eq!(
        restarted.reserve_batch(&f.user, &raw).await.unwrap(),
        receipt
    );
    let cancelled = f.cancel(&saved).await;
    assert_eq!(cancelled.disposition, Disposition::CancelledBeforePlan);
    let lookup = restarted
        .lookup_operation(&f.user, f.actor, receipt.operation_id)
        .await
        .unwrap();
    assert_eq!(lookup.receipt, receipt);
    assert_eq!(
        lookup.current_reservation.unwrap().state,
        ReservationState::CancelledBeforePlan
    );
    let a = f.owner.inspect_actor(&f.user, f.actor).await.unwrap();
    assert!(a.current_reservation.is_none());
    assert_eq!(
        f.owner
            .history(&f.user, f.actor, None, 16)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let after: Value =
        sqlx::query_scalar("SELECT to_jsonb(g) FROM global_supervisor_conversation g")
            .fetch_one(&f.state.store.pool)
            .await
            .unwrap();
    assert_eq!(after, before);
    for table in [
        "execution_attempts",
        "agent_launch_bindings",
        "mailbox_receipts",
        "mailbox_deliveries",
    ] {
        assert_eq!(f.count(table).await, 0, "{table}");
    }
}
#[tokio::test]
async fn current_owner_full_identity_and_role_are_required_for_every_read_and_mutation() {
    let f = Fixture::new(true).await;
    f.seed("Queued").await;
    let raw = f.reserve_request(&f.preview(0).await);
    let mut viewer = f.user.clone();
    viewer.role = Role::Viewer;
    assert!(matches!(
        f.owner.inspect_actor(&viewer, f.actor).await,
        Err(AppError::Forbidden(_))
    ));
    assert!(matches!(
        f.owner.reserve_batch(&viewer, &raw).await,
        Err(AppError::Forbidden(_))
    ));
    for field in 0..3 {
        let mut stale = f.user.clone();
        match field {
            0 => stale.issuer = Some("changed".into()),
            1 => stale.subject = Some("changed".into()),
            _ => stale.email = "changed@localhost".into(),
        };
        assert!(matches!(
            f.owner.inspect_actor(&stale, f.actor).await,
            Err(AppError::Forbidden(_))
        ));
    }
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.user.id)
        .execute(&f.state.store.pool)
        .await
        .unwrap();
    assert!(matches!(
        f.owner.reserve_batch(&f.user, &raw).await,
        Err(AppError::Forbidden(_))
    ));
    assert_eq!(f.count("central_supervisor_reservations").await, 0);
}
async fn blocked(pool: &sqlx::PgPool) {
    for _ in 0..50 {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='aq-supervisor-conversation' AND wait_event_type='Lock')").fetch_one(pool).await.unwrap();
        if waiting {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("actual reservation query did not reach its authority lock");
}
#[tokio::test]
async fn identity_and_policy_changes_while_lock_waiting_refuse_before_admission() {
    let f = Fixture::new(true).await;
    f.seed("Queued").await;
    let raw = f.reserve_request(&f.preview(0).await);
    let pool = f.db.pool().await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(f.user.id)
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let service = f.owner.clone();
    let user = f.user.clone();
    let attempt = raw.clone();
    let pending = tokio::spawn(async move { service.reserve_batch(&user, &attempt).await });
    blocked(&pool).await;
    sqlx::query("UPDATE users SET subject='replacement' WHERE id=$1")
        .bind(f.user.id)
        .execute(&mut *held)
        .await
        .unwrap();
    held.commit().await.unwrap();
    assert!(matches!(
        pending.await.unwrap(),
        Err(AppError::Forbidden(_))
    ));
    assert_eq!(f.count("central_supervisor_reservations").await, 0);
    sqlx::query("UPDATE users SET subject=$2 WHERE id=$1")
        .bind(f.user.id)
        .bind(&f.user.subject)
        .execute(&pool)
        .await
        .unwrap();
    let mut held = pool.begin().await.unwrap();
    authorize(&mut held, &f.user).await.unwrap();
    definition(&mut held, None, false).await.unwrap();
    let locked = actor(&mut held, f.actor, true).await.unwrap().unwrap();
    let service = f.owner.clone();
    let user = f.user.clone();
    let attempt = raw.clone();
    let pending = tokio::spawn(async move { service.reserve_batch(&user, &attempt).await });
    blocked(&pool).await;
    writes::disable_for_test(&mut held, &locked, &f.user)
        .await
        .unwrap();
    held.commit().await.unwrap();
    let refusal = pending.await.unwrap().unwrap();
    assert_eq!(refusal.disposition, Disposition::NotReserved);
    assert_eq!(refusal.reason, Some(Reason::RevisionChanged));
    assert_eq!(f.count("central_supervisor_reservations").await, 0);
    assert_eq!(f.owner.reserve_batch(&f.user, &raw).await.unwrap(), refusal);
}
#[tokio::test]
async fn count_utf8_body_budget_partial_scans_and_head_integrity_are_truthful() {
    let f = Fixture::new(true).await;
    for _ in 0..5 {
        f.seed(&"🙂".repeat(4096)).await;
    }
    let p = f.preview(0).await;
    assert_eq!(p.batch.as_ref().unwrap().messages.len(), 4);
    assert_eq!(p.body_bytes, 65536);
    assert_eq!(p.coverage, Coverage::Partial);
    assert!(p.has_more);
    assert_eq!(p.scanned_through_cursor, 4);
    let (_, receipt) = f.reserve().await;
    f.cancel(receipt.reservation.as_ref().unwrap()).await;
    let last = f.preview(4).await;
    assert_eq!(last.batch.unwrap().messages.len(), 1);
    assert_eq!(last.coverage, Coverage::Complete);
    for _ in 0..18 {
        f.seed("tiny").await;
    }
    let p = f.preview(5).await;
    assert_eq!(p.batch.unwrap().messages.len(), 16);
    assert_eq!(p.coverage, Coverage::Partial);
    let mailbox = key(f.conversation).unwrap();
    sqlx::query("UPDATE mailbox_heads SET next_cursor=1 WHERE mailbox_key=$1")
        .bind(&mailbox)
        .execute(&f.state.store.pool)
        .await
        .unwrap();
    let a = f.owner.inspect_actor(&f.user, f.actor).await.unwrap();
    assert!(matches!(
        f.owner
            .preview_batch(
                &f.user,
                PreviewRequest {
                    actor_id: f.actor,
                    expected_actor_revision: a.revision,
                    scan_after: 0,
                    through_cursor: None
                }
            )
            .await,
        Err(AppError::IdentityUnavailable(_))
    ));
}
#[tokio::test]
async fn immutable_original_global_rows_and_receipts_preserve_remote_behavior() {
    let f = Fixture::new(true).await;
    let id = f.seed("AAAA").await;
    for statement in [
        "UPDATE mailbox_messages SET body='BBBB' WHERE message_id=$1",
        "DELETE FROM mailbox_messages WHERE message_id=$1",
    ] {
        assert!(
            sqlx::query(statement)
                .bind(id)
                .execute(&f.state.store.pool)
                .await
                .is_err()
        );
    }
    assert!(sqlx::query("UPDATE mailbox_operations SET response=jsonb_set(response,'{body_digest}','\"sha256:bad\"') WHERE response->>'message_id'=$1").bind(id.to_string()).execute(&f.state.store.pool).await.is_err());
    assert!(
        sqlx::query("TRUNCATE mailbox_operations")
            .execute(&f.state.store.pool)
            .await
            .is_err()
    );
    let legacy = Uuid::new_v4();
    sqlx::query("INSERT INTO mailbox_heads(mailbox_key,next_cursor) VALUES('legacy',2)")
        .execute(&f.state.store.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mailbox_messages(message_id,mailbox_key,cursor,thread_id,sender,recipient,payload_digest,body,created_at_ms) VALUES($1,'legacy',1,$2,'{}','{\"kind\":\"task\"}','legacy','original',1000)").bind(legacy).bind(Uuid::new_v4()).execute(&f.state.store.pool).await.unwrap();
    sqlx::query("UPDATE mailbox_messages SET body='legacy edit' WHERE message_id=$1")
        .bind(legacy)
        .execute(&f.state.store.pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE mailbox_messages SET recipient=$2 WHERE message_id=$1")
            .bind(legacy)
            .bind(super::super::recipient(f.conversation))
            .execute(&f.state.store.pool)
            .await
            .is_err()
    );
    let key_alias = sqlx::query("UPDATE mailbox_messages SET mailbox_key=$2 WHERE message_id=$1")
        .bind(legacy)
        .bind(key(f.conversation).unwrap())
        .execute(&f.state.store.pool)
        .await
        .unwrap_err();
    assert!(
        key_alias
            .to_string()
            .contains("global supervisor original mail is immutable"),
        "guard must refuse before a cursor/unique collision: {key_alias}"
    );
    assert_eq!(f.preview(0).await.status, PreviewStatus::Ready);
}
#[tokio::test]
async fn concurrent_admission_cas_and_cross_kind_operation_identity_do_not_double_claim() {
    let f = Fixture::new(true).await;
    f.seed("One").await;
    let p = f.preview(0).await;
    let first = f.reserve_request(&p);
    let second = f.reserve_request(&p);
    let (a, b) = tokio::join!(
        f.owner.reserve_batch(&f.user, &first),
        f.owner.reserve_batch(&f.user, &second)
    );
    let results = [a.unwrap(), b.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|r| r.disposition == Disposition::Reserved)
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| r.disposition == Disposition::NotReserved)
            .count(),
        1
    );
    assert_eq!(f.count("central_supervisor_reservations").await, 1);
    let original = results
        .into_iter()
        .find(|r| r.disposition == Disposition::Reserved)
        .unwrap();
    let mut cancel = Cancel {
        operation_id: original.operation_id,
        actor_id: f.actor,
        reservation_id: original.reservation.unwrap().id,
        expected_reservation_revision: 1,
    };
    assert!(matches!(
        f.owner
            .cancel_reservation(&f.user, &serde_json::to_vec(&cancel).unwrap())
            .await,
        Err(AppError::Conflict(_))
    ));
    cancel.operation_id = Uuid::new_v4();
    let cancelled = f
        .owner
        .cancel_reservation(&f.user, &serde_json::to_vec(&cancel).unwrap())
        .await
        .unwrap();
    assert_eq!(cancelled.disposition, Disposition::CancelledBeforePlan);
}
#[test]
fn versioned_schema_is_frozen_and_runtime_not_advertised() {
    let generated = schema();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/central-reservation/v1/schema.json");
    if std::env::var_os("AQ_UPDATE_RESERVATION_SCHEMA").is_some() {
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&generated).unwrap()),
        )
        .unwrap();
    } else {
        let frozen: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(generated, frozen);
    }
    assert_eq!(generated["x-execution-authority"], false);
}
#[tokio::test]
async fn sql_requires_exact_original_reserve_and_cancel_operation_views() {
    let f = Fixture::new(true).await;
    f.seed("Exact source").await;
    let (_, first) = f.reserve().await;
    let old = first.reservation.unwrap();
    f.cancel(&old).await;
    let preview = f.preview(0).await;
    let request: Reserve = serde_json::from_slice(&f.reserve_request(&preview)).unwrap();
    let mut tx = f.state.store.pool.begin().await.unwrap();
    let a = bound_actor(&mut tx, &f.user, f.actor, true).await.unwrap();
    let (raw, digest) = command("reserve", &f.user, &request).unwrap();
    sqlx::query("INSERT INTO central_supervisor_reservations(reservation_id,actor_id,reserve_operation_id,admission_actor_revision,policy_revision,revision,state,through_cursor,batch_digest,message_count,body_bytes) SELECT $2,actor_id,$3,$4,policy_revision,1,'awaiting_prepared_plan',through_cursor,batch_digest,message_count,body_bytes FROM central_supervisor_reservations WHERE reservation_id=$1").bind(old.id).bind(request.reservation_id).bind(request.operation_id).bind(a.revision+1).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO central_supervisor_mail_members(actor_id,reservation_id,ordinal,message_id,mailbox_key,cursor,payload_digest) SELECT actor_id,$2,ordinal,message_id,mailbox_key,cursor,payload_digest FROM central_supervisor_mail_members WHERE reservation_id=$1").bind(old.id).bind(request.reservation_id).execute(&mut *tx).await.unwrap();
    let mut wrong = old.clone();
    wrong.reserve_operation_id = request.operation_id;
    wrong.admission_actor_revision = a.revision as u64 + 1;
    let result = Receipt {
        contract: CONTRACT.into(),
        actor_id: f.actor,
        conversation_id: f.conversation,
        operation_id: request.operation_id,
        initiating_user_id: f.user.id,
        request_digest: digest,
        actor_revision: a.revision as u64 + 1,
        policy_revision: a.policy_revision as u64,
        disposition: Disposition::Reserved,
        reason: None,
        reservation: Some(wrong),
    };
    writes::finish(
        &mut tx,
        &a,
        &f.user,
        writes::Mutation {
            kind: "reserve",
            request: &raw,
            receipt: &result,
        },
        Some(request.reservation_id),
        None,
    )
    .await
    .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("original reserve operation proof"),
        "{error}"
    );
    assert_eq!(f.count("central_supervisor_reservations").await, 1);
    let (_, active) = f.reserve().await;
    let active = active.reservation.unwrap();
    let mut tx = f.state.store.pool.begin().await.unwrap();
    let a = bound_actor(&mut tx, &f.user, f.actor, true).await.unwrap();
    let configure = Configure {
        operation_id: Uuid::new_v4(),
        actor_id: f.actor,
        expected_actor_revision: a.revision as u64,
        policy: a.policy.clone(),
    };
    let (raw, digest) = command("configure", &f.user, &configure).unwrap();
    sqlx::query("UPDATE central_supervisor_reservations SET state='cancelled_before_plan',revision=2,cancel_operation_id=$2,cancel_actor_revision=$3 WHERE reservation_id=$1").bind(active.id).bind(configure.operation_id).bind(a.revision+1).execute(&mut *tx).await.unwrap();
    let result = Receipt {
        contract: CONTRACT.into(),
        actor_id: f.actor,
        conversation_id: f.conversation,
        operation_id: configure.operation_id,
        initiating_user_id: f.user.id,
        request_digest: digest,
        actor_revision: a.revision as u64 + 1,
        policy_revision: a.policy_revision as u64 + 1,
        disposition: Disposition::Configured,
        reason: None,
        reservation: None,
    };
    writes::finish(
        &mut tx,
        &a,
        &f.user,
        writes::Mutation {
            kind: "configure",
            request: &raw,
            receipt: &result,
        },
        None,
        Some(&configure.policy),
    )
    .await
    .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("original cancel operation proof"),
        "{error}"
    );
    assert_eq!(
        f.owner
            .inspect_actor(&f.user, f.actor)
            .await
            .unwrap()
            .current_reservation
            .unwrap()
            .id,
        active.id
    );
}
// A valid synthetic persisted metadata-only journal at the lifetime boundary.
// It records no Run, host effect, provider call or assistant reply. Actual actor,
// policy and full initiating human context come from this disposable database.
async fn journal_fixture(count: usize, padded: bool) -> Fixture {
    let f = Fixture::new(false).await;
    let mut entries = Vec::new();
    let mut total = 0i64;
    for n in 1..=count {
        let request = Configure {
            operation_id: Uuid::new_v4(),
            actor_id: f.actor,
            expected_actor_revision: 0,
            policy: f.policy(true),
        };
        let (raw, digest) = command("configure", &f.user, &request).unwrap();
        let result = Receipt {
            contract: CONTRACT.into(),
            actor_id: f.actor,
            conversation_id: f.conversation,
            operation_id: request.operation_id,
            initiating_user_id: f.user.id,
            request_digest: digest,
            actor_revision: n as u64,
            policy_revision: 1,
            disposition: if n == 1 {
                Disposition::Configured
            } else {
                Disposition::NotConfigured
            },
            reason: if n == 1 {
                None
            } else {
                Some(Reason::RevisionChanged)
            },
            reservation: None,
        };
        let result = String::from_utf8(canonical(&result).unwrap()).unwrap();
        let charge = if padded {
            CONTROL_BYTES
        } else {
            (raw.len() + result.len() + 8192) as i64
        };
        total += charge;
        entries.push((request.operation_id, raw, result, charge));
    }
    let mut tx = f.state.store.pool.begin().await.unwrap();
    let policy = canonical(&f.policy(true)).unwrap();
    sqlx::query("INSERT INTO central_supervisor_actors(actor_id,conversation_id,revision,policy_revision,policy_bytes,policy_digest,approved_by,approved_identity,retained_operations,retained_bytes) VALUES($1,$2,$3,1,$4,$5,$6,$7,$3,$8)").bind(f.actor).bind(f.conversation).bind(count as i64).bind(String::from_utf8(policy.clone()).unwrap()).bind(Digest::of(&policy).as_str()).bind(f.user.id).bind(identity(&f.user)).bind(total).execute(&mut *tx).await.unwrap();
    let mut query = sqlx::QueryBuilder::<Postgres>::new(
        "INSERT INTO central_supervisor_operations(actor_id,operation_id,kind,actor_revision,initiating_user_id,request_bytes,request_digest,result_bytes,charged_bytes) ",
    );
    query.push_values(
        entries.iter().enumerate(),
        |mut b, (n, (id, request, result, charge))| {
            b.push_bind(f.actor)
                .push_bind(*id)
                .push_bind("configure")
                .push_bind(n as i64 + 1)
                .push_bind(f.user.id)
                .push_bind(request)
                .push_bind(Digest::of(request.as_bytes()).as_str().to_string())
                .push_bind(result)
                .push_bind(*charge);
        },
    );
    query.build().execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    f
}
#[tokio::test]
async fn last_operation_slot_preserves_exact_cancellation_but_no_other_writer() {
    let f = journal_fixture(4094, false).await;
    f.seed("Last available reserve").await;
    let (raw, accepted) = f.reserve().await;
    let reservation = accepted.reservation.unwrap();
    assert_eq!(
        f.owner
            .inspect_actor(&f.user, f.actor)
            .await
            .unwrap()
            .retained_operations,
        4095
    );
    assert!(matches!(
        f.configure(false, 4095).await,
        Err(AppError::Maintenance(_))
    ));
    let wrong = Cancel {
        operation_id: Uuid::new_v4(),
        actor_id: f.actor,
        reservation_id: Uuid::new_v4(),
        expected_reservation_revision: 1,
    };
    assert!(matches!(
        f.owner
            .cancel_reservation(&f.user, &serde_json::to_vec(&wrong).unwrap())
            .await,
        Err(AppError::Maintenance(_))
    ));
    let cancel = Cancel {
        operation_id: Uuid::new_v4(),
        actor_id: f.actor,
        reservation_id: reservation.id,
        expected_reservation_revision: 1,
    };
    let bytes = serde_json::to_vec(&cancel).unwrap();
    let cancelled = f.owner.cancel_reservation(&f.user, &bytes).await.unwrap();
    assert_eq!(cancelled.disposition, Disposition::CancelledBeforePlan);
    assert_eq!(
        f.owner
            .inspect_actor(&f.user, f.actor)
            .await
            .unwrap()
            .retained_operations,
        4096
    );
    assert_eq!(
        f.owner.cancel_reservation(&f.user, &bytes).await.unwrap(),
        cancelled
    );
    assert_eq!(
        f.owner
            .reserve_batch(&f.user, &raw)
            .await
            .unwrap()
            .disposition,
        Disposition::Reserved
    );
    assert!(
        f.owner
            .inspect_actor(&f.user, f.actor)
            .await
            .unwrap()
            .current_reservation
            .is_none()
    );
    let fresh = f.reserve_request(&f.preview(0).await);
    assert!(matches!(
        f.owner.reserve_batch(&f.user, &fresh).await,
        Err(AppError::Maintenance(_))
    ));
}
#[tokio::test]
async fn protected_control_bytes_allow_cancel_after_ordinary_byte_capacity_exhausts() {
    let f = journal_fixture(510, true).await;
    f.seed("Byte budget").await;
    let (_, accepted) = f.reserve().await;
    let saved = accepted.reservation.unwrap();
    let mut reached = false;
    for _ in 0..32 {
        let a = f.owner.inspect_actor(&f.user, f.actor).await.unwrap();
        match f.configure(true, a.revision).await {
            Ok(_) => {}
            Err(AppError::Maintenance(_)) => {
                reached = true;
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(reached);
    let before = f.owner.inspect_actor(&f.user, f.actor).await.unwrap();
    assert!(before.retained_bytes <= (MAX_BYTES - CONTROL_BYTES) as u64);
    let cancelled = f.cancel(&saved).await;
    assert_eq!(cancelled.disposition, Disposition::CancelledBeforePlan);
    let after = f.owner.inspect_actor(&f.user, f.actor).await.unwrap();
    assert!(after.retained_bytes > before.retained_bytes);
    assert!(after.retained_bytes <= MAX_BYTES as u64);
    assert!(after.current_reservation.is_none());
}
#[test]
fn closed_commands_reject_fake_execution_fields_duplicates_and_fractional_revisions() {
    let value = json!({"operation_id":Uuid::new_v4(),"actor_id":Uuid::new_v4(),"expected_actor_revision":0,"policy":{"schema_version":1,"kind":"global_supervisor_reservation","actor_id":Uuid::new_v4(),"conversation_id":Uuid::new_v4(),"enabled":true}});
    for field in [
        "session",
        "generation",
        "dispatch_id",
        "profile",
        "credentials",
    ] {
        let mut bad = value.clone();
        bad[field] = json!("unsupported");
        assert!(parse::<Configure>(&serde_json::to_vec(&bad).unwrap()).is_err());
    }
    let text = value.to_string().replace(
        "\"expected_actor_revision\":0",
        "\"expected_actor_revision\":0,\"expected_actor_revision\":0",
    );
    assert!(parse::<Configure>(text.as_bytes()).is_err());
    let mut bad = value;
    bad["expected_actor_revision"] = json!(0.5);
    assert!(parse::<Configure>(&serde_json::to_vec(&bad).unwrap()).is_err());
    assert!(parse::<Configure>(&vec![b' '; 16385]).is_err());
}

#[test]
fn normative_documents_decode_and_bind_exact_reservation_request_and_batch() {
    let examples: Value = serde_json::from_slice(
        &std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../protocol/central-reservation/v1/examples.json"),
        )
        .unwrap(),
    )
    .unwrap();
    for value in examples.as_object().unwrap().values() {
        let _: Document = serde_json::from_value(value.clone()).unwrap();
    }
    let user = User {
        id: Uuid::from_u128(7),
        email: "owner@localhost".into(),
        role: Role::Owner,
        issuer: Some("dev".into()),
        subject: Some("owner@localhost".into()),
    };
    let request: Reserve = serde_json::from_value(examples["reserve"]["data"].clone()).unwrap();
    let receipt: Receipt =
        serde_json::from_value(examples["reserve_receipt"]["data"].clone()).unwrap();
    assert_eq!(
        command("reserve", &user, &request).unwrap().1,
        receipt.request_digest
    );
    let saved = receipt.reservation.unwrap();
    assert_eq!(saved.batch.digest().unwrap(), saved.batch_digest);
    let mut preview = examples["preview"]["data"].clone();
    let digest: Digest = serde_json::from_value(
        preview
            .as_object_mut()
            .unwrap()
            .remove("selection_digest")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(hash(&preview).unwrap(), digest);
}
