//! Actual disposable SQL owners, bounded synthetic Graph/Git and private bytes.
//! This qualifies profile joining, not providers, physical hosts or runtime eligibility.
use crate::common;
#[path = "../common/phase_refresh_execution.rs"]
mod fixture;
use aq_admission::preparation_refresh_v1 as phase;
use aq_server::profiled_execution::phase_refresh_execution as owner;
use fixture::{Prepared, auth, prepared};
use serde_json::{Value, json};
use uuid::Uuid;

async fn stored(f: &Prepared) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    sqlx::query_as("SELECT request_bytes,receipt_bytes,provenance_bytes FROM profiled_phase_execution_grants WHERE attempt_id=$1")
        .bind(f.prepared.original.binding.attempt_id).fetch_one(&f.c.env.env.pool).await.unwrap()
}
async fn issue(f: &Prepared) -> phase::ExecutionDelivery {
    let reply = owner::prepare(
        &f.c.env.env.state,
        &auth(&f.c).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    reply
        .validate_for(&f.prepared, &f.a, &f.q, &f.p, reply.delivery.nonce)
        .unwrap();
    reply
}
async fn assess(f: &Prepared, sequence: u64, state: &str, cooldown: u64, unrelated: bool) {
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM intelligence_registry WHERE singleton")
            .fetch_one(&f.c.env.env.pool)
            .await
            .unwrap();
    let mut q = f.c.env.manual.clone();
    q["operation_id"] = json!(Uuid::new_v4());
    q["expected_revision"] = json!(revision);
    q["sequence"] = json!(sequence);
    q["state"] = json!(state);
    q["cooldown_ms"] = json!(cooldown);
    if unrelated {
        q["scope"]["model"] = json!("unrelated-model");
    }
    let (status, body) = common::selection::call(
        &aq_server::build_admin_router(f.c.env.env.state.clone()),
        "/api/intelligence/observations",
        Some(q),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, 200, "{body}");
}
async fn refuse_preserving_issue(f: &Prepared, original: &(Vec<u8>, Vec<u8>, Vec<u8>)) {
    assert!(
        owner::prepare(
            &f.c.env.env.state,
            &auth(&f.c).await,
            &f.prepared,
            Uuid::new_v4()
        )
        .await
        .is_err()
    );
    assert_eq!(&stored(f).await, original);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_phase_execution_grants")
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn refreshed_positive_before_issue_preserves_actual_four_registry_reads() {
    let f = prepared(true).await;
    fixture::assert_unissued(&f.c).await;
    let selected = f.p.choice.registry.clone();
    let accepted = f.a.lease.registry_at_accept.clone();
    assess(&f, 3, "available", 0, false).await;
    let grant = issue(&f).await;
    let fact = grant
        .receipt
        .confirmation
        .facts
        .iter()
        .find_map(|f| match &f.body.value {
            phase::FactValue::Registry {
                selected_registry,
                accepted_registry,
                preflight_registry,
                current_registry,
                ..
            } => Some((
                selected_registry,
                accepted_registry,
                preflight_registry,
                current_registry,
            )),
            _ => None,
        })
        .unwrap();
    assert_eq!(fact.0, &selected);
    assert_eq!(fact.1, &accepted);
    assert!(
        phase::registry_revision(fact.2).unwrap() > phase::registry_revision(&accepted).unwrap()
    );
    phase::validate_registry_chain(fact.0, fact.1, fact.2, fact.3).unwrap();
    let stored = stored(&f).await;
    let provenance: Value = serde_json::from_slice(&stored.2).unwrap();
    assert_eq!(provenance["preflight_registry"], json!(fact.2));
    assert_eq!(provenance["current_registry"], json!(fact.3));
    // Neither proposal nor accepted receipt is rewritten to manufacture continuity.
    assert_eq!(f.p.choice.registry, selected);
    assert_eq!(f.a.lease.registry_at_accept, accepted);
}

#[tokio::test]
async fn unrelated_positive_advance_replays_exact_original_issue_without_extension() {
    let f = prepared(true).await;
    let first = issue(&f).await;
    let original = stored(&f).await;
    assess(&f, 1, "available", 0, true).await;
    let replay = issue(&f).await;
    assert_eq!(json!(first.receipt), json!(replay.receipt));
    assert_ne!(first.delivery.nonce, replay.delivery.nonce);
    assert_eq!(
        first.delivery.clock.expires_at_ms,
        replay.delivery.clock.expires_at_ms
    );
    assert!(replay.delivery.clock.server_time_ms >= first.delivery.clock.server_time_ms);
    assert_eq!(stored(&f).await, original);
}

#[tokio::test]
async fn newer_negative_and_active_cooldown_deny_exact_old_receipt_replay() {
    for (state, cooldown) in [("authentication_failed", 0), ("available", 10_000)] {
        let f = prepared(true).await;
        issue(&f).await;
        let original = stored(&f).await;
        assess(&f, 3, state, cooldown, false).await;
        refuse_preserving_issue(&f, &original).await;
    }
}

#[tokio::test]
async fn actual_private_backing_change_denies_replay_without_new_receipt() {
    let f = prepared(true).await;
    issue(&f).await;
    let original = stored(&f).await;
    let registry = f.c.env.env.state.config.intelligence.as_ref().unwrap();
    let path = registry
        .config
        .credentials
        .iter()
        .find(|c| c.credential.profile == f.a.lease.materials[0].credential_profile)
        .unwrap()
        .secret_file
        .as_ref()
        .unwrap();
    std::fs::write(path, b"changed synthetic private source after issue").unwrap();
    refuse_preserving_issue(&f, &original).await;
}

#[tokio::test]
async fn stale_health_and_expired_original_grant_are_never_revived() {
    for expire_grant in [false, true] {
        eprintln!("refresh_stale_setup expire_original_grant={expire_grant}");
        let f = prepared(true).await;
        let first = issue(&f).await;
        let original = stored(&f).await;
        let at = if expire_grant {
            first.receipt.grant.expires_at_ms
        } else {
            first.receipt.issued_at_ms + 5_001
        };
        assert!(expire_grant || at < first.receipt.grant.expires_at_ms);
        f.c.controlled_clock
            .as_ref()
            .unwrap()
            .store(at as i64, std::sync::atomic::Ordering::SeqCst);
        refuse_preserving_issue(&f, &original).await;
    }
}

#[tokio::test]
async fn actual_config_change_and_credential_rotation_deny_original_issue_replay() {
    for rotate_credential in [false, true] {
        let mut f = prepared(true).await;
        issue(&f).await;
        let original = stored(&f).await;
        let mut config = (*f.c.env.env.state.config).clone();
        let mut intelligence = config.intelligence.as_ref().unwrap().config.clone();
        intelligence.revision += 1;
        if rotate_credential {
            for credential in &mut intelligence.credentials {
                credential.credential.revision += 1;
            }
        }
        config.intelligence = Some(std::sync::Arc::new(intelligence.load().unwrap()));
        f.c.env.env.state.config = std::sync::Arc::new(config);
        let (status, body) = common::selection::call(
            &aq_server::build_admin_router(f.c.env.env.state.clone()),
            "/api/intelligence/register",
            Some(json!({})),
            "owner@localhost",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        refuse_preserving_issue(&f, &original).await;
    }
}

#[tokio::test]
async fn retired_actual_project_principal_policy_denies_replay_from_old_daemon() {
    let f = prepared(true).await;
    issue(&f).await;
    let original = stored(&f).await;
    let kind: String = sqlx::query_scalar("SELECT authority_kind FROM intelligence_decisions WHERE decision_id=(SELECT decision_id FROM profiled_launches WHERE attempt_id=$1)")
        .bind(f.prepared.original.binding.attempt_id).fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(kind, "project_policy");
    let mut installer = f.c.env.env.state.clone();
    let mut config = (*installer.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    profiled.revision += 1;
    for policy in &mut profiled.projects {
        policy.enabled = false;
    }
    config.profiled_execution = Some(std::sync::Arc::new(profiled));
    installer.config = std::sync::Arc::new(config);
    aq_server::profiled_execution::policies::install(&installer)
        .await
        .unwrap();
    refuse_preserving_issue(&f, &original).await;
}

#[tokio::test]
async fn actual_external_read_preserves_monotonic_positive_and_fences_negative_or_rollback() {
    use std::sync::atomic::Ordering;
    for mode in ["positive", "negative", "rollback"] {
        let rollback = mode == "rollback";
        let f = prepared(true).await;
        issue(&f).await;
        let original = stored(&f).await;
        let issued_revision: i64 =
            sqlx::query_scalar("SELECT revision FROM intelligence_registry WHERE singleton")
                .fetch_one(&f.c.env.env.pool)
                .await
                .unwrap();
        if rollback {
            assess(&f, 1, "available", 0, true).await;
        }
        f.c.mode.store(6, Ordering::SeqCst);
        let state = f.c.env.env.state.clone();
        let node = auth(&f.c).await;
        let request = f.prepared.clone();
        let task =
            tokio::spawn(
                async move { owner::prepare(&state, &node, &request, Uuid::new_v4()).await },
            );
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            f.c.observation_entered.acquire(),
        )
        .await
        .expect("actual replay must reach the unlocked external observation")
        .unwrap()
        .forget();
        if rollback {
            // Deliberate disposable-DB corruption: restore the older aggregate
            // and matching observation rows while retaining original issue and
            // real replay preflight evidence. This isolates the preflight floor.
            let mut tx = f.c.env.env.pool.begin().await.unwrap();
            sqlx::query("UPDATE intelligence_registry SET revision=$1 WHERE singleton")
                .bind(issued_revision)
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("DELETE FROM intelligence_observations WHERE observation#>>'{scope,model}'='unrelated-model'")
                .execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
        } else {
            // Genuine current owner mutation completes while no issuer locks
            // are held, and must fence the original receipt after I/O returns.
            if mode == "positive" {
                assess(&f, 1, "available", 0, true).await;
            } else {
                assess(&f, 3, "authentication_failed", 0, false).await;
            }
        }
        f.c.mode.store(0, Ordering::SeqCst);
        f.c.observation_resume.add_permits(1);
        let result = task.await.unwrap();
        if mode == "positive" {
            let replay =
                result.expect("unrelated advance during I/O must not poison original replay");
            assert_eq!(
                json!(replay.receipt),
                serde_json::from_slice::<Value>(&original.1).unwrap()
            );
        } else {
            assert!(result.is_err());
        }
        assert_eq!(stored(&f).await, original);
    }
}

#[tokio::test]
async fn original_live_grant_replays_after_preparation_retrieval_expiry_without_renewing_it() {
    let f = prepared(true).await;
    let first = issue(&f).await;
    let original = stored(&f).await;
    let at = f.a.lease.expires_at_ms + 1;
    assert!(at < first.receipt.grant.expires_at_ms);
    assert!(at > f.a.lease.materials[0].expires_at_ms);
    f.c.controlled_clock
        .as_ref()
        .unwrap()
        .store(at as i64, std::sync::atomic::Ordering::SeqCst);
    // A new explicit synthetic operator assessment, not a timestamp rewrite of
    // the original selected observation or renewal of the material lease.
    assess(&f, 3, "available", 0, false).await;
    let replay = issue(&f).await;
    assert_eq!(json!(first.receipt), json!(replay.receipt));
    assert_eq!(replay.delivery.clock.server_time_ms, at);
    assert_eq!(stored(&f).await, original);
}

/// Fix round 2: the owner's switch reaches the phase health check. With no
/// observation row, Prepare issues while the switch is on and its replay stays
/// healthy past observation_max_age_ms (5 s here); with the switch off (or
/// unset) Prepare is refused.
async fn switched(f: &Prepared, on: Option<bool>) {
    let pool = &f.c.env.env.pool;
    sqlx::query("DELETE FROM intelligence_observations")
        .execute(pool)
        .await
        .unwrap();
    let Some(on) = on else { return };
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email='owner@localhost'")
        .fetch_one(pool)
        .await
        .unwrap();
    let now = (f.c.env.env.state.clock)();
    let mut c = pool.acquire().await.unwrap();
    aq_machine_control::set_owner_availability(&mut c, on, 1, owner, Uuid::new_v4(), now)
        .await
        .unwrap();
}
async fn refused(f: &Prepared) -> bool {
    let auth = auth(&f.c).await;
    owner::prepare(&f.c.env.env.state, &auth, &f.prepared, Uuid::new_v4())
        .await
        .is_err()
}

#[tokio::test]
async fn owner_switch_on_prepares_and_stays_healthy_without_an_observation() {
    let f = prepared(true).await;
    switched(&f, Some(true)).await;
    let first = issue(&f).await;
    f.c.controlled_clock.as_ref().unwrap().store(
        first.receipt.issued_at_ms as i64 + 5_001,
        std::sync::atomic::Ordering::SeqCst,
    );
    let replay = issue(&f).await;
    assert_eq!(json!(first.receipt), json!(replay.receipt));
}

#[tokio::test]
async fn owner_switch_off_or_unset_refuses_prepare_without_an_observation() {
    for on in [Some(false), None] {
        let f = prepared(true).await;
        switched(&f, on).await;
        assert!(
            refused(&f).await,
            "switch {on:?}: no attestation, no Prepare"
        );
        fixture::assert_unissued(&f.c).await;
    }
}

/// One fixture observation row, kept as a template before `switched` deletes them.
async fn template(f: &Prepared) -> Value {
    sqlx::query_scalar("SELECT observation FROM intelligence_observations LIMIT 1")
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap()
}
/// Fresh worker `quota_unavailable` feedback from `node` on the chosen credential.
async fn negative_from(f: &Prepared, mut o: Value, node: Uuid) {
    let now = (f.c.env.env.state.clock)().timestamp_millis();
    o["observation_id"] = json!(Uuid::new_v4());
    o["scope"]["node_id"] = json!(node);
    o["scope"]["model"] = json!(f.p.choice.model.model);
    o["source"] = json!({"kind":"worker","node_id":node,"boot_id":Uuid::new_v4(),"attempt_id":Uuid::new_v4(),"selection_digest":aq_selection::Digest::of(b"s")});
    o["state"] = json!("quota_unavailable");
    o["received_at_ms"] = json!(now);
    o["expires_at_ms"] = json!(now + 60_000);
    o["cooldown_until_ms"] = json!(0);
    sqlx::query("INSERT INTO intelligence_observations(scope_source_digest,sequence,observation) VALUES($1,1,$2)")
        .bind(format!("worker-{node}"))
        .bind(o)
        .execute(&f.c.env.env.pool)
        .await
        .unwrap();
}

/// Fix round 3: the switch's attestation is held off only by this node's
/// feedback (as in selection): another node's negative on the credential does
/// not refuse this node's Prepare; this node's own negative does. The issued
/// attestation carries the registry revision.
#[tokio::test]
async fn only_this_nodes_feedback_holds_the_switch_attestation_off() {
    let f = prepared(true).await;
    let o = template(&f).await;
    switched(&f, Some(true)).await;
    negative_from(&f, o.clone(), Uuid::new_v4()).await;
    issue(&f).await;
    let provenance: Value = serde_json::from_slice(&stored(&f).await.2).unwrap();
    let registry: i64 = sqlx::query_scalar("SELECT revision FROM intelligence_registry")
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap();
    let attested = provenance["availability"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["sequence"].as_u64().unwrap() >= 1 << 62)
        .expect("the switch's attestation is in the provenance");
    assert_eq!(attested["revision"], json!(registry));
    drop(f);
    let f = prepared(true).await;
    let o = template(&f).await;
    switched(&f, Some(true)).await;
    negative_from(&f, o, auth(&f.c).await.id()).await;
    assert!(refused(&f).await, "this node's own negative holds it off");
    fixture::assert_unissued(&f.c).await;
}

/// Fix round 3: at the 1,024-row observation cap the attestation would be the
/// 1,025th entry; Prepare is refused cleanly and nothing is issued. (The
/// provenance byte bound already refuses a table this large; the cap check
/// keeps the stored availability within the history bound regardless.)
#[tokio::test]
async fn full_observation_table_refuses_cleanly_with_the_switch_on() {
    let f = prepared(true).await;
    let pool = &f.c.env.env.pool;
    sqlx::query("INSERT INTO intelligence_observations(scope_source_digest,sequence,observation) SELECT 'pad-'||g, 1, o.observation || jsonb_build_object('observation_id', gen_random_uuid(), 'scope', o.observation->'scope' || jsonb_build_object('model', 'pad-'||g)) FROM generate_series(1, 1024 - (SELECT count(*) FROM intelligence_observations)::int) g, (SELECT observation FROM intelligence_observations LIMIT 1) o")
        .execute(pool)
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM intelligence_observations")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(rows, 1024);
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email='owner@localhost'")
        .fetch_one(pool)
        .await
        .unwrap();
    let now = (f.c.env.env.state.clock)();
    let mut c = pool.acquire().await.unwrap();
    aq_machine_control::set_owner_availability(&mut c, true, 1, owner, Uuid::new_v4(), now)
        .await
        .unwrap();
    drop(c);
    assert!(refused(&f).await, "1,024 rows plus the attestation");
    fixture::assert_unissued(&f.c).await;
}
