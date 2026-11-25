use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_session_starts_under_central_permit() {
    let case = Case::new().await;
    let host = case.controller();
    let run = case.start(&host).await;
    assert_eq!(run.run.phase(), Phase::RunningReported);
    assert_eq!(run.run.plan().session, case.central.session);
    let observed = host.observe(case.central.subject.actor_id()).await.unwrap();
    assert_eq!(observed.phase, aq_session_host::HostPhase::Running);
    assert_eq!(observed.permit_sequence, 1);
    assert_eq!(
        observed.permit_deadline_boot_ms,
        run.run.permit().unwrap().expires_boot_ms
    );
    let directory = case
        .root
        .path()
        .join(observed.guardian.intent.launch_id.to_string());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        std::fs::read(directory.join("delivered")).unwrap(),
        b"supervisor startup\n"
    );
    let stopped = host.stop(case.central.subject.actor_id()).await.unwrap();
    assert_eq!(stopped.run.phase(), Phase::StoppedReported);
    assert!(directory.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_restart_resumes_without_duplicate_session() {
    let case = Case::new().await;
    let first = case.controller();
    let started = case.start(&first).await;
    let identity = first
        .observe(case.central.subject.actor_id())
        .await
        .unwrap()
        .guardian;
    drop(first);
    let second = case.controller();
    let resumed = second
        .reconcile(case.central.subject.actor_id())
        .await
        .unwrap();
    assert_eq!(resumed.run.plan().session, started.run.plan().session);
    assert_eq!(resumed.run.permit(), started.run.permit());
    let replay = case.start(&second).await;
    assert_eq!(replay.run.plan().generation, 1);
    assert_eq!(
        second
            .observe(case.central.subject.actor_id())
            .await
            .unwrap()
            .guardian,
        identity
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM central_host_launches")
        .fetch_one(&case.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    second.stop(case.central.subject.actor_id()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_start_has_one_guardian_and_cancel_prevents_revival() {
    let case = Case::new().await;
    let a = case.controller();
    let b = case.controller();
    let (x, y) = tokio::join!(
        a.start(
            aq_supervisor_host::Startup {
                central: case.central.clone(),
                host: case.host.clone(),
                input: b"supervisor startup\n".to_vec()
            },
            30_000
        ),
        b.start(
            aq_supervisor_host::Startup {
                central: case.central.clone(),
                host: case.host.clone(),
                input: b"supervisor startup\n".to_vec()
            },
            30_000
        )
    );
    assert_eq!(x.unwrap().run.plan(), y.unwrap().run.plan());
    assert_eq!(
        a.observe(case.central.subject.actor_id())
            .await
            .unwrap()
            .guardian,
        b.observe(case.central.subject.actor_id())
            .await
            .unwrap()
            .guardian
    );
    a.stop(case.central.subject.actor_id()).await.unwrap();
    assert!(
        b.renew(case.central.subject.actor_id(), 1, 30_000)
            .await
            .is_err()
    );
    assert!(
        b.start(
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatched_host_plan_never_creates_a_writer() {
    let mut case = Case::new().await;
    let host = case.controller();
    case.host.input.dispatch_id = Uuid::new_v4();
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
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM central_host_launches")
        .fetch_one(&case.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_supervisor_is_stopped_and_scratch_is_retained() {
    let case = Case::new().await;
    let host = case.controller();
    host.start(
        aq_supervisor_host::Startup {
            central: case.central.clone(),
            host: case.host.clone(),
            input: b"supervisor startup\n".to_vec(),
        },
        2000,
    )
    .await
    .unwrap();
    let id = host
        .observe(case.central.subject.actor_id())
        .await
        .unwrap()
        .guardian
        .intent
        .launch_id;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let stopped = host
        .reconcile(case.central.subject.actor_id())
        .await
        .unwrap();
    assert_eq!(stopped.run.phase(), Phase::StoppedReported);
    assert!(
        case.root
            .path()
            .join(id.to_string())
            .join("delivered")
            .exists()
    );
    assert!(
        host.renew(case.central.subject.actor_id(), 1, 30_000)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successor_requires_stop_and_retains_prior_generation() {
    let mut case = Case::new().await;
    let host = case.controller();
    case.start(&host).await;
    let actor = case.central.subject.actor_id();
    let original = case.central.clone();
    case.central.generation = 2;
    if let SessionIdentity::Central {
        session_id,
        generation,
        ..
    } = &mut case.central.session
    {
        *session_id = Uuid::new_v4();
        *generation = 2;
    }
    case.central.dispatch_id = Uuid::new_v4();
    case.central.host_operation_id = Uuid::new_v4();
    case.central.batch.messages.truncate(1);
    case.central.batch.messages[0].message_id = Uuid::new_v4();
    case.central.batch.messages[0].cursor = 100;
    case.central.batch.through_cursor = 100;
    case.host.session = case.central.session.clone();
    case.host.input.dispatch_id = case.central.dispatch_id;
    case.host.input.host_operation_id = case.central.host_operation_id;
    case.host.input.batch.digest = case.central.batch.digest().unwrap();
    let now = aq_supervisor_host::clock().unwrap();
    case.host.admitted_boot_ms = now.boot_ms;
    case.host.deadline_boot_ms = now.boot_ms + 60_000;
    case.central.profiles.host_plan.digest = case.host.digest().unwrap();
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
    host.stop(actor).await.unwrap();
    let now = aq_supervisor_host::clock().unwrap();
    case.host.admitted_boot_ms = now.boot_ms;
    case.host.deadline_boot_ms = now.boot_ms + 60_000;
    case.central.profiles.host_plan.digest = case.host.digest().unwrap();
    let next = case.start(&host).await;
    assert_eq!(next.run.plan().generation, 2);
    let bytes: String = sqlx::query_scalar(
        "SELECT state_bytes FROM central_session_generations WHERE actor_id=$1 AND generation=1",
    )
    .bind(actor)
    .fetch_one(&case.pool)
    .await
    .unwrap();
    let Document::State(old) = decode(bytes.as_bytes()).unwrap() else {
        panic!()
    };
    assert_eq!(old.plan(), &original);
    assert_eq!(old.phase(), Phase::StoppedReported);
    host.stop(actor).await.unwrap();
}
