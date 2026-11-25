use super::*;
use aq_session_protocol::{ControllerLease, InputDisposition, InputReceipt, canonical_digest};
use aq_terminal_relay::{api::MutationReply, draft::Effect, epoch};

#[tokio::test]
async fn epoch_receipts_roll_over_bounded_sql_window_and_retired_input_cannot_replay() {
    let (mut f, dir, mut r) = running().await;
    enable_epoch_input(&mut f, &dir, &mut r).await;
    let node = authenticated(&f).await;
    let lease = acquire_control(&f, &r, f.actor).await.lease.unwrap();
    let (_, fence) = pending_fence(&f, &r, &node).await;
    let mut upload = fence_upload(&r, 0, &fence);
    aq_server::terminal::peer::control_upload(&f.state, &node, &upload)
        .await
        .unwrap();
    upload.fence_acks.clear();
    let permit = open_epoch(&f, &r, &lease).await;
    let mut first = None;
    let mut last_retired = 0;
    for sequence in 1..=80 {
        let request = sequenced_input(&r, &lease, &permit, sequence);
        if sequence == 1 {
            first = Some(request.clone());
        }
        service::mutate(
            &f.state,
            f.actor,
            &service::Mutation::EpochInput(request.clone()),
        )
        .await
        .unwrap();
        let command = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
            .await
            .unwrap()
            .effects
            .into_iter()
            .find_map(|e| {
                if let Effect::EpochInput(c) = e {
                    Some(c)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(command.sequence, sequence);
        upload.exchange_id = Uuid::new_v4();
        upload.epoch_receipts = vec![epoch::Receipt {
            epoch_id: permit.epoch_id,
            sequence,
            receipt: InputReceipt {
                terminal: r.terminal.clone(),
                operation_id: request.input.operation_id,
                command_digest: canonical_digest(&command.input).unwrap(),
                disposition: InputDisposition::Applied,
            },
        }];
        let ack = aq_server::terminal::peer::control_upload(&f.state, &node, &upload)
            .await
            .unwrap();
        last_retired = ack.retirements[0].retired_through;
    }
    assert_eq!(last_retired, 16);
    assert_persisted_window(&f, &r).await;
    assert!(
        service::mutate(
            &f.state,
            f.actor,
            &service::Mutation::EpochInput(first.unwrap())
        )
        .await
        .is_err()
    );
}

async fn enable_epoch_input(f: &mut Fixture, dir: &tempfile::TempDir, r: &mut Register) {
    let mut cfg = (*f.state.config).clone();
    cfg.terminals = Some(relay_config(dir.path().join("key"), true));
    f.state.config = Arc::new(cfg);
    aq_server::terminal::persistence::install(&f.state)
        .await
        .unwrap();
    r.input_mode = InputMode::EpochV1;
    r.capability = Text::new(epoch::CAPABILITY).unwrap();
    r.advertised_capabilities.push(Capability {
        id: Text::new(epoch::CAPABILITY).unwrap(),
        version: 1,
    });
    register(f, r).await;
}

fn sequenced_input(
    r: &Register,
    lease: &ControllerLease,
    permit: &epoch::Permit,
    sequence: u64,
) -> epoch::Request {
    epoch::Request {
        version: 1,
        capability: Text::new(epoch::CAPABILITY).unwrap(),
        epoch_id: permit.epoch_id,
        sequence,
        input: aq_session_api::InputRequest {
            terminal: r.terminal.clone(),
            operation_id: epoch::operation_id(permit, sequence).unwrap(),
            lease_id: lease.lease_id,
            expected_controller_generation: lease.generation,
            expected_session_revision: 1,
            expires_at_ms: NOW + 2000,
            bytes: vec![sequence as u8],
            // A lone ESC (sequence 27) is only a complete packet as the Escape key.
            escape_key: sequence == 0x1b,
        },
    }
}

async fn assert_persisted_window(f: &Fixture, r: &Register) {
    let pool = f.db.pool().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM terminal_input_receipts WHERE terminal_id=$1"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        64
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM terminal_operations WHERE terminal_id=$1"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        3
    );
}

async fn open_epoch(f: &Fixture, r: &Register, lease: &ControllerLease) -> epoch::Permit {
    let reply = service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::OpenEpoch(epoch::Open {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            expected_controller_generation: lease.generation,
            lease_id: lease.lease_id,
            expires_at_ms: NOW + 2000,
        }),
    )
    .await
    .unwrap();
    let MutationReply::Epoch { permit, .. } = reply else {
        panic!("expected an open input epoch")
    };
    permit
}
