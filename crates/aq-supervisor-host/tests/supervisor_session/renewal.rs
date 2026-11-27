use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn physical_renewal_requires_next_sequence_and_later_deadline() {
    let case = Case::new().await;
    let host = case.controller();
    host.start(
        aq_supervisor_host::Startup {
            central: case.central.clone(),
            host: case.host.clone(),
            input: b"supervisor startup\n".to_vec(),
        },
        5000,
    )
    .await
    .unwrap();
    let before = host.observe(case.central.subject.actor_id()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let next = host
        .renew(case.central.subject.actor_id(), 1, 10000)
        .await
        .unwrap();
    // Physical renewal stages renew.request; the guardian applies it on its
    // next loop, so wait for it instead of racing a single observation.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let after = loop {
        let observed = host.observe(case.central.subject.actor_id()).await.unwrap();
        if observed.permit_sequence >= 2 || tokio::time::Instant::now() >= deadline {
            break observed;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(after.guardian, before.guardian);
    assert_eq!(after.permit_sequence, 2);
    assert!(after.permit_deadline_boot_ms > before.permit_deadline_boot_ms);
    assert_eq!(
        after.permit_deadline_boot_ms,
        next.run.permit().unwrap().expires_boot_ms
    );
    assert!(
        host.renew(case.central.subject.actor_id(), 1, 10000)
            .await
            .is_err()
    );
    host.stop(case.central.subject.actor_id()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_guardian_blocks_successor_and_retains_scratch() {
    let case = Case::new().await;
    let host = case.controller();
    case.start(&host).await;
    let observed = host.observe(case.central.subject.actor_id()).await.unwrap();
    use aq_session_host::ownership::{signal, verified_pidfd};
    let guardian = verified_pidfd(&observed.guardian.process).unwrap();
    signal(&guardian, libc::SIGKILL).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        host.reconcile(case.central.subject.actor_id())
            .await
            .is_err()
    );
    assert_eq!(
        host.inspect(case.central.subject.actor_id())
            .await
            .unwrap()
            .run
            .phase(),
        Phase::StopUncertain
    );
    assert!(
        case.root
            .path()
            .join(observed.guardian.intent.launch_id.to_string())
            .exists()
    );
    assert!(
        host.start(
            aq_supervisor_host::Startup {
                central: case.central.clone(),
                host: case.host.clone(),
                input: b"supervisor startup\n".to_vec()
            },
            30_000
        )
        .await
        .is_err()
    );
    // Test-only cleanup by exact pidfd. This is deliberately not stopped proof.
    if let Some(pane) = observed.pane {
        for p in [pane.pane, pane.server] {
            if let Ok(fd) = verified_pidfd(&p) {
                let _ = signal(&fd, libc::SIGKILL);
            }
        }
    }
}
