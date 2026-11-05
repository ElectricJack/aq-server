//! Registered hot-migration qualification; no production grant producer.
mod common;
use aq_server::execution::authority::hot::HotState;
use common::profiled::{fixture, insert};
use serde_json::json;
use sqlx::Row;

#[tokio::test]
async fn adoption_preserves_actual_authority_and_guards_snapshot_revisions_and_seals() {
    let f = fixture().await;
    let attempt = insert(&f).await;
    let pool = f.db.pool().await;
    let hot = HotState::from_compat(&f.profiled).unwrap();
    let mut invalid = hot.clone();
    invalid.execution_deadline_ms = Some(0);
    assert!(invalid.validate_against(&f.profiled.offer.binding).is_err());
    invalid = hot.clone();
    invalid.status = aq_server::execution::profiled::Status::Granted;
    assert!(invalid.validate_against(&f.profiled.offer.binding).is_err());
    invalid = hot.clone();
    invalid.retention = aq_server::execution::profiled::Retention::NotRequired;
    assert!(invalid.validate_against(&f.profiled.offer.binding).is_err());

    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,999,999,999)")
        .bind(attempt).bind(serde_json::to_value(&hot).unwrap()).execute(&pool).await.unwrap();
    for _ in 0..2 {
        let mut tx = pool.begin().await.unwrap();
        let view = aq_server::execution::authority::load_locked(
            &f.state,
            &mut tx,
            &f.profiled.offer.binding.session,
            aq_server::execution::authority::Policy::IdentityOnly,
        )
        .await
        .unwrap();
        assert_eq!(view.status, hot.status);
        tx.rollback().await.unwrap();
    }
    let stats = f.state.store.profiled_cache_stats().unwrap();
    assert_eq!((stats.0, stats.2, stats.3), (1, 1, 1));
    let revisions = sqlx::query("SELECT record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1")
        .bind(attempt).fetch_one(&pool).await.unwrap();
    assert_eq!(revisions.get::<i64, _>("record_revision"), 1);
    sqlx::query("UPDATE profiled_authority_hot SET record_revision=999,authority_revision=999,grant_revision=999 WHERE attempt_id=$1")
        .bind(attempt).execute(&pool).await.unwrap();
    let revision: i64 = sqlx::query_scalar(
        "SELECT authority_revision FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(revision, 1);
    let mut changed = serde_json::to_value(&f.profiled).unwrap();
    changed["revoked"] = json!(true);
    assert!(
        sqlx::query("UPDATE execution_attempts SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(changed)
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{revoked}','true') WHERE attempt_id=$1").bind(attempt).execute(&pool).await.unwrap();
    let revision: i64 = sqlx::query_scalar(
        "SELECT authority_revision FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(revision, 2);
    assert!(
        sqlx::query("DELETE FROM intelligence_attempt_selections WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .is_err()
    );
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(held); // Revocation alone never supplies verified stop/source/effect release.
    assert!(
        sqlx::query("UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .is_err()
    );
    let release_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_verified_releases WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(release_count, 0); // No synthetic verified-release producer.
    assert!(sqlx::query("UPDATE profiled_authority_hot SET state=state||jsonb_build_object('oversized',repeat('x',16384)) WHERE attempt_id=$1").bind(attempt).execute(&pool).await.is_err());
    sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{execution_deadline_ms}','1000001') WHERE attempt_id=$1").bind(attempt).execute(&pool).await.unwrap();
    assert!(sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{execution_deadline_ms}','1000002') WHERE attempt_id=$1").bind(attempt).execute(&pool).await.is_err());
}

#[tokio::test]
async fn immutable_cache_is_exact_keyed_count_and_byte_bounded() {
    use aq_server::execution::authority::cache::{Facts, Key, VerifiedCache};
    use uuid::Uuid;
    let f = fixture().await;
    let binding = &f.profiled.offer.binding;
    let mut facts = Facts {
        binding: binding.clone(),
        project_id: f.profiled.offer.manifest.bindings.project_id,
        task_id: f.profiled.offer.manifest.bindings.task_id,
        graph: f.profiled.graph.clone(),
        terminal_policy: f.profiled.offer.manifest.terminal.clone(),
        source: f.profiled.source.clone(),
    };
    let mut key = Key {
        attempt_id: binding.attempt_id,
        decision_id: f.decision,
        source_observation_id: Uuid::new_v4(),
        execution_spec_digest: binding.execution_spec_digest.as_str().into(),
        launch_manifest_digest: binding.launch_manifest_digest.clone(),
        selection_digest: aq_selection::digest(&f.selection).unwrap(),
    };
    let first_key = key.clone();
    let mut cache = VerifiedCache::default();
    assert!(cache.get(&key).is_none());
    assert!(cache.insert_verified(key.clone(), facts.clone()));
    assert_eq!(cache.get(&key).unwrap().project_id, facts.project_id);
    let mut mismatch = key.clone();
    mismatch.selection_digest = aq_session_protocol::Digest::of(b"different");
    assert!(cache.get(&mismatch).is_none());
    let mut thousand = Vec::new();
    let started = std::time::Instant::now();
    for _ in 0..1000 {
        key.attempt_id = Uuid::new_v4();
        facts.binding.attempt_id = key.attempt_id;
        if let aq_session_protocol::SessionIdentity::Remote { attempt_id, .. } =
            &mut facts.binding.session
        {
            *attempt_id = key.attempt_id;
        }
        assert!(cache.insert_verified(key.clone(), facts.clone()));
        thousand.push(key.clone());
    }
    let hits = thousand
        .iter()
        .filter(|key| cache.get(key).is_some())
        .count();
    let (count, bytes) = cache.retained();
    eprintln!(
        "compact cache1000: second-pass hits={hits}/1000 entries={count} logical_bytes={bytes} elapsed_ms={}",
        started.elapsed().as_millis()
    );
    assert_eq!(hits, 1000);
    assert!(count <= VerifiedCache::MAX_ENTRIES && bytes <= VerifiedCache::MAX_BYTES);
    for _ in 0..1100 {
        key.attempt_id = Uuid::new_v4();
        facts.binding.attempt_id = key.attempt_id;
        if let aq_session_protocol::SessionIdentity::Remote { attempt_id, .. } =
            &mut facts.binding.session
        {
            *attempt_id = key.attempt_id;
        }
        assert!(cache.insert_verified(key.clone(), facts.clone()));
    }
    assert!(cache.get(&first_key).is_none());
    key.execution_spec_digest = "x".repeat(100_000);
    assert!(!cache.insert_verified(key, facts));
}

#[tokio::test]
async fn immutable_cache_hit_never_caches_renewal_revocation_or_expiry() {
    use aq_server::execution::{
        authority::{
            cache::{Facts, Key, VerifiedCache},
            hot::{self, LaunchOwner},
        },
        profiled::{Status, Stop},
    };
    use aq_session_protocol::ProfiledGrant;
    use common::profiled::NOW;
    use uuid::Uuid;
    let f = fixture().await;
    let binding = f.profiled.offer.binding.clone();
    let key = Key {
        attempt_id: binding.attempt_id,
        decision_id: f.decision,
        source_observation_id: Uuid::new_v4(),
        execution_spec_digest: binding.execution_spec_digest.as_str().into(),
        launch_manifest_digest: binding.launch_manifest_digest.clone(),
        selection_digest: aq_selection::digest(&f.selection).unwrap(),
    };
    let facts = Facts {
        binding: binding.clone(),
        project_id: f.profiled.offer.manifest.bindings.project_id,
        task_id: f.profiled.offer.manifest.bindings.task_id,
        graph: f.profiled.graph.clone(),
        terminal_policy: f.profiled.offer.manifest.terminal.clone(),
        source: f.profiled.source.clone(),
    };
    let mut cache = VerifiedCache::default();
    assert!(cache.insert_verified(key.clone(), facts));
    let mut state = HotState::from_compat(&f.profiled).unwrap();
    state.status = Status::Granted;
    state.stop = Stop::Running;
    state.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    state.execution_deadline_ms = Some(NOW + 60000);
    state.grant = Some(ProfiledGrant {
        binding,
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 1000,
    });
    let first = hot::project(
        state.clone(),
        cache.get(&key).unwrap(),
        &key,
        (2, 2, 1),
        false,
        true,
    )
    .unwrap();
    first.active_at(NOW).unwrap();
    assert!(first.active_at(NOW + 1000).is_err());
    state.grant.as_mut().unwrap().expires_at_ms = NOW + 2000;
    let renewed = hot::project(
        state.clone(),
        cache.get(&key).unwrap(),
        &key,
        (3, 2, 2),
        false,
        true,
    )
    .unwrap();
    renewed.active_at(NOW + 1000).unwrap();
    assert_eq!(renewed.authority_revision, first.authority_revision);
    let revoked =
        hot::project(state, cache.get(&key).unwrap(), &key, (3, 2, 2), true, true).unwrap();
    assert!(revoked.active_at(NOW).is_err());
}

#[tokio::test]
async fn actual_common_loader_reads_hot_renewal_revocation_and_compatibility_fallback() {
    use aq_server::execution::{
        authority::{self, AuthorityError, Policy, hot::LaunchOwner},
        profiled::{Status, Stop},
    };
    use aq_session_protocol::ProfiledGrant;
    use common::profiled::{NOW, artifact};
    use uuid::Uuid;
    // Qualify the original hot/compatibility storage contract with its actual
    // pre-settlement schema. Synthetic release records cannot satisfy owner39.
    let mut f = aq_server::test_support::with_store_profile(27, fixture()).await;
    f.profiled.offer.manifest.terminal.input_enabled = false;
    f.profiled.offer.manifest.terminal.max_input_bytes = 64;
    f.profiled.offer.manifest.terminal.max_pending_input_bytes = 128;
    f.profiled.offer.manifest.terminal.max_frame_bytes = 256;
    f.profiled.offer.manifest.terminal.replay_bytes = 1024;
    f.profiled.offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&f.profiled.offer.manifest).unwrap();
    let attempt = insert(&f).await;
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    let fallback = authority::load_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        Policy::IdentityOnly,
    )
    .await
    .unwrap();
    assert_eq!(fallback.status, Status::Offered);
    tx.rollback().await.unwrap();
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(hot)).execute(&pool).await.unwrap();
    hot.status = Status::Preparing;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    hot.status = Status::Granted;
    hot.stop = Stop::Running;
    hot.execution_deadline_ms = Some(NOW + 60000);
    hot.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 1000,
    });
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let first = authority::load_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        Policy::TerminalActive,
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(first.grant_expires_at_ms, NOW + 1000);
    assert_eq!(
        first.terminal_policy.as_ref(),
        Some(&f.profiled.offer.manifest.terminal)
    );
    assert_eq!(f.state.store.profiled_cache_stats().unwrap().3, 1);
    hot.grant.as_mut().unwrap().expires_at_ms = NOW + 2000;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let renewed = authority::load_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        Policy::TerminalActive,
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(renewed.grant_expires_at_ms, NOW + 2000);
    assert_eq!(renewed.terminal_policy, first.terminal_policy);
    assert_eq!(renewed.authority_revision, first.authority_revision);
    assert!(renewed.grant_revision > first.grant_revision);
    assert_eq!(f.state.store.profiled_cache_stats().unwrap().2, 1);
    let mut changed_owner = hot.clone();
    changed_owner.owner.as_mut().unwrap().guardian_id = Uuid::new_v4();
    assert!(
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(json!(changed_owner))
            .execute(&pool)
            .await
            .is_err()
    );
    let active = hot.clone();
    let mut changed_id = hot.clone();
    changed_id.grant.as_mut().unwrap().grant_id = Uuid::new_v4();
    assert!(
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(json!(changed_id))
            .execute(&pool)
            .await
            .is_err()
    );
    hot.status = Status::Cancelling;
    hot.revoked = true;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        authority::load_locked(
            &f.state,
            &mut tx,
            &f.profiled.offer.binding.session,
            Policy::TerminalActive
        )
        .await,
        Err(AuthorityError::Revoked)
    ));
    tx.rollback().await.unwrap();
    let snapshot: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(snapshot["status"], json!("offered")); // Never consulted as current authority after adoption.
    hot.status = Status::Finished;
    let stop = artifact("synthetic-stop-owner-receipt", b"synthetic-stop");
    let retained = artifact("synthetic-source-owner-receipt", b"synthetic-source");
    let effects = artifact("synthetic-effect-owner-receipt", b"synthetic-effects");
    hot.stop = Stop::Stopped {
        receipt: stop.clone(),
    };
    hot.retention = aq_server::execution::profiled::Retention::Retained {
        receipt: retained.clone(),
    };
    hot.effects = aq_server::execution::profiled::Effects::Quiescent {
        receipt: effects.clone(),
    };
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    let mut revived = hot.clone();
    revived.stop = Stop::Running;
    assert!(
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(json!(revived))
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(json!(active))
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{revoked}','false') WHERE attempt_id=$1").bind(attempt).execute(&pool).await.is_err());
    let mut changed_status = hot.clone();
    changed_status.status = Status::Unknown;
    let transition_error =
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(json!(changed_status))
            .execute(&pool)
            .await
            .unwrap_err();
    assert!(transition_error.to_string().contains("cannot reactivate"));
    // Fixture-only trusted owner record: this tests storage fencing, not real
    // physical stop or source/effect qualification and has no HTTP producer.
    sqlx::query("INSERT INTO profiled_verified_releases(attempt_id,release_id,stop_operation_id,stop_evidence_digest,host_owner_qualification,source_receipt_digest,effect_receipt_digest,verified_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(attempt).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(stop.digest.as_str()).bind(json!(artifact("synthetic-qualified-host",b"synthetic-only"))).bind(retained.digest.as_str()).bind(effects.digest.as_str()).bind(NOW as i64).execute(&pool).await.unwrap();
    sqlx::query("UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1")
        .bind(attempt)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE execution_attempts SET holds_task=true WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM profiled_verified_releases WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .is_err()
    );
    // Deliberate test-only corruption bypasses SQL monotonic guards. Common
    // authority still refuses a released claim; old release proof cannot rearm it.
    sqlx::raw_sql("ALTER TABLE profiled_authority_hot DISABLE TRIGGER profiled_hot_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(active))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("ALTER TABLE profiled_authority_hot ENABLE TRIGGER profiled_hot_guard")
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        authority::load_locked(
            &f.state,
            &mut tx,
            &f.profiled.offer.binding.session,
            Policy::TerminalActive
        )
        .await,
        Err(AuthorityError::Revoked)
    ));
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let historical = authority::load_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        Policy::TerminalHistorical,
    )
    .await
    .unwrap();
    assert!(!historical.holds_task);
    assert!(historical.active_at(NOW).is_err());
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn production_typed_spec_checks_actual_sealed_selection_and_source() {
    use aq_admission::{execution_spec::ProfiledExecutionSpec, *};
    use aq_server::execution::authority::{self, AuthorityError, Policy};
    use aq_session_protocol::{ContextDelivery, ContextRef};
    use common::profiled::{NOW, artifact, insert_typed};
    for case in 0..3 {
        let wrong_selection = case == 1;
        let mut f = fixture().await;
        let task_context = artifact("synthetic-task-context", b"synthetic task");
        f.profiled
            .offer
            .manifest
            .context
            .references
            .push(ContextRef {
                artifact: task_context.clone(),
                provenance: Text::new("synthetic-test-only-task").unwrap(),
                delivery: ContextDelivery::Startup,
                bytes: 14,
                tokens: 4,
                required: true,
            });
        let selection_digest = aq_selection::digest(&f.selection).unwrap();
        f.profiled.offer.manifest.selection_explanation = ArtifactRef {
            id: Text::new("sealed-selection").unwrap(),
            version: Text::new("1").unwrap(),
            digest: selection_digest.clone(),
        };
        f.profiled.offer.binding.launch_manifest_digest =
            aq_session_protocol::canonical_digest(&f.profiled.offer.manifest).unwrap();
        let mut spec = ProfiledExecutionSpec {
            schema_version: 1,
            lifecycle: transport::lifecycle_binding(&f.profiled.offer).unwrap(),
            project_revision: f.profiled.offer.manifest.bindings.project_revision,
            task: TaskRevision {
                workspace_id: f.selection.lineage.workspace_id,
                task_id: f.selection.lineage.task_id,
                version: f.profiled.offer.manifest.bindings.task_revision,
                content_revision: 1,
            },
            source: SourceAdmission {
                schema_version: 1,
                project_id: f.selection.lineage.project_id,
                project_revision: 1,
                repository_id: f.profiled.source.repository_id,
                repository_binding_generation: f.profiled.source.binding_generation,
                policy: artifact("synthetic-source-policy", b"immutable-base-policy"),
                selection: SourceSelection::Immutable {
                    object: ObjectId {
                        format: ObjectFormat::Sha1,
                        hex: f.profiled.source.base.hex.clone(),
                    },
                },
                resolved_object: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: f.profiled.source.base.hex.clone(),
                },
                observed_at_ms: NOW,
                expires_at_ms: NOW + 30000,
            },
            source_artifact: f.profiled.offer.manifest.bindings.source.clone(),
            outcome_artifact: f.profiled.offer.manifest.bindings.outcome.clone(),
            graph_access_profile: f
                .profiled
                .offer
                .manifest
                .bindings
                .graph_access_profile
                .clone(),
            task_observation: artifact("synthetic-task-observation", b"graph-context"),
            task_context,
            selection_digest: if wrong_selection {
                Digest::of(b"wrong sealed choice")
            } else {
                selection_digest
            },
        };
        if case == 2 {
            spec.source.resolved_object.hex = "b".repeat(40);
            spec.source.selection = SourceSelection::Immutable {
                object: spec.source.resolved_object.clone(),
            };
        }
        f.profiled.offer.binding.execution_spec_digest = spec.artifact().unwrap().digest;
        insert_typed(&f, &spec).await;
        let pool = f.db.pool().await;
        let mut tx = pool.begin().await.unwrap();
        let result = authority::load_locked(
            &f.state,
            &mut tx,
            &f.profiled.offer.binding.session,
            Policy::IdentityOnly,
        )
        .await;
        if case != 0 {
            assert!(matches!(result, Err(AuthorityError::InvalidState)));
        } else {
            result.unwrap();
        }
        tx.rollback().await.unwrap();
    }
}
