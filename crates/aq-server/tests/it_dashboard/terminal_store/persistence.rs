use super::*;

#[tokio::test]
async fn encrypted_registration_and_exact_controller_retry_survive_sql_reload() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    register(&f, &r).await;
    let node = authenticated(&f).await;
    let mut changed_registration = r.clone();
    changed_registration
        .advertised_capabilities
        .push(Capability {
            id: Text::new("test.additional").unwrap(),
            version: 1,
        });
    assert!(
        service::register(&f.state, &node, &changed_registration)
            .await
            .is_err()
    );
    let info = service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    assert_eq!(info.terminal, r.terminal);
    let request = control_request(&r, 4_000);
    let first = service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Control(request.clone()),
    )
    .await
    .unwrap();
    let retry = service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Control(request.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(retry).unwrap()
    );
    let mut changed = request;
    changed.expires_at_ms += 1;
    assert!(
        service::mutate(&f.state, f.actor, &service::Mutation::Control(changed))
            .await
            .is_err()
    );
    let row: (Vec<u8>, i64) = sqlx::query_as(
        "SELECT ciphertext,ledger_revision FROM terminal_sessions WHERE terminal_id=$1",
    )
    .bind(r.terminal.terminal_id)
    .fetch_one(&f.db.pool().await)
    .await
    .unwrap();
    assert!(!row.0.windows(10).any(|v| v == b"controller"));
    assert_eq!(row.1, 1);
}

#[tokio::test]
async fn altered_ciphertext_and_terminal_replacement_fail_closed() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let mut changed = r.clone();
    changed.terminal.epoch += 1;
    let node = authenticated(&f).await;
    assert!(service::register(&f.state, &node, &changed).await.is_err());
    sqlx::query("UPDATE terminal_sessions SET ciphertext=decode(repeat('00',32),'hex') WHERE terminal_id=$1").bind(r.terminal.terminal_id).execute(&f.db.pool().await).await.unwrap();
    assert!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
}

async fn reload_keys(f: &mut Fixture, dir: &std::path::Path, active: &str, ids: &[&str]) {
    let mut cfg = (*f.state.config).clone();
    cfg.terminals = Some(Arc::new(Loaded {
        keyring: Keyring::load(&ProtectionConfig {
            active_key_id: active.into(),
            keys: ids
                .iter()
                .map(|id| KeyFile {
                    id: (*id).into(),
                    file: dir.join(if *id == "one" { "key" } else { "key-two" }),
                })
                .collect(),
        })
        .unwrap(),
        epoch_input: false,
        runtime_id: Uuid::new_v4(),
        runtime: Default::default(),
    }));
    f.state.config = Arc::new(cfg);
    aq_server::terminal::persistence::install(&f.state)
        .await
        .unwrap();
}

#[tokio::test]
async fn actual_stored_ciphertext_requires_old_key_and_rewrites_under_rotated_key() {
    let (mut f, dir, r) = running().await;
    register(&f, &r).await;
    let watch = || service::Mutation::Watch(watch_request(&r, 10_000));
    service::mutate(&f.state, f.actor, &watch()).await.unwrap();
    let pool = f.db.pool().await;
    let old:(i32,String,Vec<u8>,Vec<u8>)=sqlx::query_as("SELECT protection_version,key_id,nonce,ciphertext FROM terminal_sessions WHERE terminal_id=$1").bind(r.terminal.terminal_id).fetch_one(&pool).await.unwrap();
    std::fs::write(dir.path().join("key-two"), [42u8; 32]).unwrap();
    std::fs::set_permissions(
        dir.path().join("key-two"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    reload_keys(&mut f, dir.path(), "two", &["two"]).await;
    assert!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
    reload_keys(&mut f, dir.path(), "two", &["one", "two"]).await;
    service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    service::mutate(&f.state, f.actor, &watch()).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT key_id FROM terminal_sessions WHERE terminal_id=$1"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "two"
    );
    reload_keys(&mut f, dir.path(), "two", &["two"]).await;
    service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    // Old authentic ciphertext is still rejected when swapped into a newer revision.
    sqlx::query("UPDATE terminal_sessions SET protection_version=$2,key_id=$3,nonce=$4,ciphertext=$5 WHERE terminal_id=$1").bind(r.terminal.terminal_id).bind(old.0).bind(old.1).bind(old.2).bind(old.3).execute(&pool).await.unwrap();
    reload_keys(&mut f, dir.path(), "two", &["one", "two"]).await;
    assert!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_controller_retries_share_one_durable_lease_and_stale_generation_refuses() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let request = service::Mutation::Control(control_request(&r, 4_000));
    let (a, b) = tokio::join!(
        service::mutate(&f.state, f.actor, &request),
        service::mutate(&f.state, f.actor, &request)
    );
    // A bounded row-lock wait may tell one caller to retry the same operation.
    // At least one concurrent caller must commit the lease before any retry.
    assert!(a.is_ok() || b.is_ok(), "neither caller acquired the lease");
    let retry_busy = |result| async {
        match result {
            Err(aq_server::error::AppError::Busy) => {
                service::mutate(&f.state, f.actor, &request).await.unwrap()
            }
            other => other.unwrap(),
        }
    };
    assert_eq!(
        serde_json::to_value(retry_busy(a).await).unwrap(),
        serde_json::to_value(retry_busy(b).await).unwrap()
    );
    let stale = service::Mutation::Control(control_request(&r, 4_000));
    assert!(service::mutate(&f.state, f.actor, &stale).await.is_err());
    let pool = f.db.pool().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM terminal_outbox WHERE terminal_id=$1 AND kind='fence'"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn owner_install_waits_for_old_transaction_then_fences_old_handlers() {
    use sqlx::Row;
    let (f, dir, r) = running().await;
    register(&f, &r).await;
    let pool = f.db.pool().await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT value FROM meta WHERE key='terminal_relay_owner' FOR SHARE")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let mut replacement = f.state.clone();
    let mut cfg = (*replacement.config).clone();
    cfg.terminals = Some(relay_config(dir.path().join("key"), false));
    replacement.config = Arc::new(cfg);
    let next = replacement.clone();
    let install =
        tokio::spawn(async move { aq_server::terminal::persistence::install(&next).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let waiting=sqlx::query("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND query LIKE 'INSERT INTO meta%' AND wait_event_type='Lock'").fetch_one(&pool).await.unwrap().get::<i64,_>(0);
        if waiting > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "install did not reach observed owner lock wait"
        );
        tokio::task::yield_now().await;
    }
    assert!(!install.is_finished());
    held.commit().await.unwrap();
    install.await.unwrap().unwrap();
    assert!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
    assert!(
        service::mutate(
            &f.state,
            f.actor,
            &service::Mutation::Control(control_request(&r, 2_000))
        )
        .await
        .is_err()
    );
    aq_server::terminal::runtime::maintenance(&replacement, Uuid::nil())
        .await
        .unwrap();
    service::inspect(&replacement, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn owner_replacement_recovers_lost_response_unknown_before_any_new_input() {
    use aq_terminal_relay::{api::MutationReply, draft::Effect};
    let (mut f, dir, r) = running().await;
    register(&f, &r).await;
    let request = claimed_input_as(&f, &r, f.actor).await;
    let old = f.state.clone();
    reload_keys(&mut f, dir.path(), "one", &["one"]).await;
    assert!(
        service::mutate(&old, f.actor, &service::Mutation::Input(request.clone()))
            .await
            .is_err()
    );
    assert!(
        service::receipt(
            &f.state,
            f.actor,
            r.terminal.terminal_id,
            request.operation_id
        )
        .await
        .is_err()
    );
    let info = service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    assert!(info.disconnected);
    assert!(!info.control.fence_acknowledged);
    aq_server::terminal::runtime::maintenance(&f.state, Uuid::nil())
        .await
        .unwrap();
    let receipt = service::receipt(
        &f.state,
        f.actor,
        r.terminal.terminal_id,
        request.operation_id,
    )
    .await
    .unwrap();
    assert_eq!(
        receipt.disposition,
        aq_session_protocol::InputDisposition::DeliveryUnknown
    );
    assert!(receipt.input_paused);
    let node = authenticated(&f).await;
    let poll = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
        .await
        .unwrap();
    assert!(
        !poll
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Input(_) | Effect::EpochInput(_)))
    );
    let MutationReply::Input(replayed) =
        service::mutate(&f.state, f.actor, &service::Mutation::Input(request))
            .await
            .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        replayed.disposition,
        aq_session_protocol::InputDisposition::DeliveryUnknown
    );
}
