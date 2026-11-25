use super::*;

#[tokio::test]
async fn unverified_release_refused_before_pull() {
    let (h, mut e, _a) = with_a().await;
    h.publish_untrusted(&manifest(B, 1, &[1]));
    let r = e.install(req(B)).await.unwrap();
    assert_refused(&r, "verif");
    assert_eq!(h.fake.count("pull"), 0);
    assert_eq!(h.fake.log(), vec![format!("fetch_release {B}")]);
    assert_active_and_idle(&e);
    assert_eq!(h.history()[0], r);
}

#[tokio::test]
async fn version_mismatch_between_request_and_manifest_refused() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    let mut r = req(B);
    r.version = Some(ver("1.2.0"));
    // Serve B's manifest under 1.2.0.
    let bytes = b.to_canonical_json();
    let sig = hex(&h.key.sign(&bytes).to_bytes());
    let mut b2 = b.clone();
    b2.version = ver("1.2.0");
    h.fake.add_release(&b2, bytes, sig.into_bytes());
    let out = e.install(r).await.unwrap();
    assert_eq!(out.outcome, Outcome::Refused, "{}", out.detail);
    assert_eq!(h.fake.count("pull"), 0);
}

#[tokio::test]
async fn incompatible_state_refused() {
    let (h, mut e, _a) = with_a().await;
    // B cannot read format 1, which the database holds.
    h.publish(&manifest(B, 2, &[2]));
    let r = e.install(req(B)).await.unwrap();
    assert_refused(&r, "state format 1");
    assert_no_calls(&h.fake, &["pull", "enter"]);
    assert_active_and_idle(&e);
}

#[tokio::test]
async fn downgrade_and_same_version_refused() {
    let (h, mut e, _a) = with_a().await;
    let r = e.install(req(A)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    h.publish(&manifest("0.9.0", 1, &[1]));
    let r = e.install(req("0.9.0")).await.unwrap();
    assert_refused(&r, "restore-previous");
    assert_eq!(h.fake.count("pull"), 0);
}

#[tokio::test]
async fn low_disk_refused_before_maintenance() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.set_free(2 * 1024 * 1024 * 1024 - 1);
    let r = e.install(req(B)).await.unwrap();
    assert_refused(&r, "free");
    assert_no_calls(&h.fake, &["pull", "enter"]);
    assert!(e.journal_state().current.is_none());
}

#[tokio::test]
async fn free_space_needs_three_times_last_backup() {
    let h = H::new();
    let a = manifest(A, 1, &[1]);
    h.publish(&a);
    let settings = EngineSettings {
        min_free_bytes: 10,
        ..h.settings()
    };
    h.fake.set_backup_size(1000);
    let mut e = h.engine_with(settings);
    assert_eq!(e.install(req(A)).await.unwrap().outcome, Outcome::Succeeded);
    h.publish(&manifest(B, 1, &[1]));
    // The A install wrote a 1000-byte dump: 3000 bytes are needed.
    h.fake.set_free(2999);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    assert!(r.detail.contains("3000"), "{}", r.detail);
    h.fake.set_free(3000);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
}

#[tokio::test]
async fn pull_failure_aborts_without_stopping_a() {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.fail("pull", 0, "registry unreachable");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted);
    assert!(r.detail.contains("registry unreachable"), "{}", r.detail);
    assert_no_calls(&h.fake, &["start_server", "stop_server", "enter"]);
    assert_eq!(h.fake.running().unwrap(), a.images.server);
    assert_active_and_idle(&e);
}

#[tokio::test]
async fn image_label_mismatch_aborts_before_maintenance() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_labels(
        &b.images.server,
        HashMap::from([("io.agent-q.state-format".to_string(), "2".to_string())]),
    );
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted);
    assert!(r.detail.contains("io.agent-q.state-format"), "{}", r.detail);
    assert_no_calls(&h.fake, &["enter", "start_server"]);
}

fn assert_refused(entry: &HistoryEntry, detail: &str) {
    assert_eq!(entry.outcome, Outcome::Refused, "{}", entry.detail);
    assert!(entry.detail.contains(detail), "{}", entry.detail);
}

fn assert_active_and_idle(engine: &TestEngine) {
    assert_eq!(active_version(engine), Some(ver(A)));
    assert!(engine.journal_state().current.is_none());
}
