use super::*;

#[tokio::test]
async fn second_request_rejected_while_in_flight() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.publish(&manifest("1.2.0", 1, &[1]));
    // Leave an update in flight (process "dies" while probing).
    h.fake.crash("wait_healthy", 0);
    assert!(e.install(req(B)).await.is_err());
    assert!(e.journal_state().current.is_some());

    // A second updater process cannot take the lock.
    assert!(lock(&h.state_dir).is_err());

    // A second request in this process is rejected without touching the runtime.
    h.fake.clear_log();
    let second = req("1.2.0");
    let r = e.install(second.clone()).await.unwrap();
    assert_eq!(r.outcome, Outcome::Rejected);
    assert_eq!(r.detail, "update_in_progress");
    assert_eq!(r.request_id, Some(second.id));
    assert!(h.fake.log().is_empty(), "{:?}", h.fake.log());
    assert_eq!(h.history()[0], r);
    // The in-flight update is untouched.
    let cur = e.journal_state().current.clone().unwrap();
    assert_eq!(cur.target.version, ver(B));
    assert_eq!(cur.step, Step::Probing);
    match e.status().state {
        UpdateState::InProgress { target, step, .. } => {
            assert_eq!(target, Some(ver(B)));
            assert_eq!(step, Step::Probing);
        }
        other => panic!("expected InProgress, got {other:?}"),
    }
}

#[tokio::test]
async fn set_available_appears_in_status() {
    let (_h, mut e, a) = with_a().await;
    assert!(e.status().available.is_none());
    let summary = e.status().active.unwrap();
    e.set_available(Some(summary.clone()));
    assert_eq!(e.status().available, Some(summary));
    assert_eq!(e.status().active.unwrap().server_image, a.images.server);
}

#[tokio::test]
async fn observer_sees_every_step_of_an_install() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    e.set_observer(Box::new(move |s: &aq_release::UpdateStatus| {
        sink.lock().unwrap().push(s.clone());
    }));
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    let seen = seen.lock().unwrap().clone();
    let steps: Vec<Step> = seen
        .iter()
        .filter_map(|s| match &s.state {
            UpdateState::InProgress { step, target, .. } => {
                assert_eq!(*target, Some(ver(B)));
                Some(*step)
            }
            _ => None,
        })
        .collect();
    assert!(steps.windows(2).all(|w| w[0] <= w[1]), "{steps:?}");
    for want in [
        Step::Requested,
        Step::Validated,
        Step::Pulled,
        Step::Maintenance,
        Step::BackedUp,
        Step::Replacing,
        Step::Probing,
        Step::Active,
    ] {
        assert!(steps.contains(&want), "{want:?} not observed in {steps:?}");
    }
    let last = seen.last().unwrap();
    assert_eq!(last.state, UpdateState::Idle);
    assert_eq!(last.active.as_ref().unwrap().version, ver(B));
    assert_eq!(last.history[0], r);
    // A rejection (history only) is observed too.
    let before = seen.len();
    let n = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let c = n.clone();
    e.set_observer(Box::new(move |_| *c.lock().unwrap() += 1));
    e.reject(None, None, "bad request".into()).unwrap();
    assert_eq!(*n.lock().unwrap(), 1);
    assert!(before > 8);
}

#[tokio::test]
async fn refresh_available_sets_only_newer_compatible_releases() {
    let (h, mut e, a) = with_a().await;
    // Channel on the active release: nothing available.
    h.fake.set_channel(a.version.clone());
    assert_eq!(e.refresh_available().await, Ok(None));
    assert!(e.status().available.is_none());
    // Newer and compatible.
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_channel(ver(B));
    let got = e.refresh_available().await.unwrap().unwrap();
    assert_eq!(got.version, ver(B));
    assert_eq!(e.status().available.unwrap().version, ver(B));
    // Fetch failure: unchanged.
    h.fake.fail(
        "fetch_release",
        h.fake.count("fetch_release"),
        "registry down",
    );
    assert!(e.refresh_available().await.is_err());
    assert_eq!(e.status().available.unwrap().version, ver(B));
    // Bad signature: unchanged.
    let c = manifest("1.2.0", 1, &[1]);
    h.publish_untrusted(&c);
    h.fake.set_channel(ver("1.2.0"));
    assert!(e.refresh_available().await.is_err());
    assert_eq!(e.status().available.unwrap().version, ver(B));
    // Newer but cannot read the installed data: not available.
    let d = manifest("1.3.0", 3, &[3]);
    h.publish(&d);
    h.fake.set_channel(ver("1.3.0"));
    assert_eq!(e.refresh_available().await, Ok(None));
    assert!(e.status().available.is_none());
    // Polling never touches the running release.
    assert_eq!(h.fake.count("start_server"), 0);
    assert_eq!(h.fake.count("pull"), 0);
}

#[tokio::test]
async fn sync_maintenance_follows_the_journal() {
    let (h, e, _a) = with_a().await;
    h.fake.set_maintenance(true);
    e.sync_maintenance().unwrap();
    assert!(!h.fake.maintenance_on(), "idle: marker removed");
}

#[tokio::test]
async fn reaching_active_clears_an_available_release_it_covers() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_channel(ver(B));
    assert_eq!(
        e.refresh_available().await.unwrap().unwrap().version,
        ver(B)
    );
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert!(
        e.status().available.is_none(),
        "B is installed, not available"
    );

    // A newer available release survives an install of an older one.
    let (h, mut e, _a) = with_a().await;
    let c = manifest("1.2.0", 1, &[1]);
    h.publish(&c);
    h.publish(&b);
    h.fake.set_channel(ver("1.2.0"));
    e.refresh_available().await.unwrap();
    e.install(req(B)).await.unwrap();
    assert_eq!(e.status().available.unwrap().version, ver("1.2.0"));
}

#[tokio::test]
async fn seeded_available_must_be_newer_than_active() {
    let (_h, mut e, a) = with_a().await;
    let mut s = e.status().active.unwrap();
    e.seed_available(Some(s.clone()));
    assert!(e.status().available.is_none(), "same as active: dropped");
    s.version = ver("0.9.0");
    e.seed_available(Some(s.clone()));
    assert!(e.status().available.is_none(), "older than active: dropped");
    s.version = ver(B);
    e.seed_available(Some(s.clone()));
    assert_eq!(e.status().available.unwrap().version, ver(B));
    assert_eq!(
        e.journal_state().active.as_ref().unwrap().version,
        a.version
    );
}

#[tokio::test]
async fn taken_request_shows_in_progress_before_the_fetch() {
    let (h, mut e, _a) = with_a().await;
    let fake = h.fake.clone();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    e.set_observer(Box::new(move |s: &aq_release::UpdateStatus| {
        sink.lock()
            .unwrap()
            .push((s.state.clone(), fake.count("fetch_release")));
    }));

    // A version request that is then refused (nothing published).
    let request = req(B);
    let r = e.install(request.clone()).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        match &seen[0] {
            (
                UpdateState::InProgress {
                    request_id,
                    target,
                    step,
                    ..
                },
                0,
            ) => {
                assert_eq!(*request_id, request.id);
                assert_eq!(*target, Some(ver(B)));
                assert_eq!(*step, Step::Requested);
            }
            other => panic!("expected InProgress before any fetch, got {other:?}"),
        }
        assert_eq!(
            seen[1],
            (UpdateState::Idle, 1),
            "idle again after the refusal"
        );
    }
    assert!(e.journal_state().current.is_none());

    // A channel request: the target is not known yet.
    seen.lock().unwrap().clear();
    let mut channel = req(B);
    channel.version = None;
    let r = e.install(channel.clone()).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    let seen = seen.lock().unwrap();
    assert!(
        matches!(
            &seen[0],
            (UpdateState::InProgress { request_id, target: None, step: Step::Requested, .. }, _)
                if *request_id == channel.id
        ),
        "{seen:?}"
    );
    assert_eq!(seen.last().unwrap().0, UpdateState::Idle);
}
