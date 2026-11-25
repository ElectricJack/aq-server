use super::*;

#[tokio::test]
async fn successful_update_a_to_b() {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert_eq!(r.from, Some(ver(A)));
    assert_eq!(r.to, Some(ver(B)));

    let s = e.journal_state();
    assert_eq!(s.active.as_ref().unwrap().version, ver(B));
    assert_eq!(s.previous.as_ref().unwrap().version, ver(A));
    assert!(s.current.is_none());
    // Persisted, not only in memory.
    let disk = Journal::read(&h.state_dir).unwrap();
    assert_eq!(disk.active.unwrap().version, ver(B));

    // Maintenance entered then left, around the backup and the replacement.
    let enter = h.fake.position("enter").unwrap();
    let backup = h.fake.position("backup").unwrap();
    let start = h.fake.position("start_server").unwrap();
    let leave = h.fake.position("leave").unwrap();
    assert!(
        enter < backup && backup < start && start < leave,
        "{:?}",
        h.fake.log()
    );
    assert!(!h.fake.maintenance_on());
    assert_eq!(
        h.fake.position("start_server"),
        h.fake
            .position(&format!("start_server {}", b.images.server))
    );
    assert_eq!(
        h.fake.count("stop_server"),
        0,
        "A is replaced in place, not stopped first"
    );

    // Backup taken, named <utc>-<from>-<to>.dump, and recorded as active_backup.
    let backup = e.journal_state().active_backup.clone().unwrap();
    assert!(backup.exists());
    let name = backup.file_name().unwrap().to_str().unwrap();
    assert!(name.ends_with(&format!("-{A}-{B}.dump")), "{name}");
    assert_eq!(
        name.len(),
        "20261030T120000Z".len() + format!("-{A}-{B}.dump").len(),
        "{name}"
    );
    assert_eq!(h.dumps().len(), 2, "first-install backup plus this one");

    assert_eq!(h.history()[0], r);
    assert_eq!(h.history().len(), 2);
    let st = e.status();
    assert_eq!(st.state, UpdateState::Idle);
    assert_eq!(st.active.unwrap().version, ver(B));
    assert_eq!(st.previous.unwrap().server_image, a.images.server);
    assert_eq!(st.history[0], r);
    assert_eq!(h.fake.running().unwrap(), b.images.server);
}

#[tokio::test]
async fn channel_request_installs_channel_release() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_channel(ver(B));
    let mut r = req(B);
    r.version = None;
    let out = e.install(r).await.unwrap();
    assert_eq!(out.outcome, Outcome::Succeeded, "{}", out.detail);
    assert_eq!(h.fake.log()[0], "fetch_release channel");
}

#[tokio::test]
async fn first_install_has_no_from_and_backs_up_none() {
    let (h, e, _a) = with_a().await;
    let first = &h.history()[0];
    assert_eq!(first.from, None);
    assert_eq!(first.outcome, Outcome::Succeeded);
    assert!(e.journal_state().previous.is_none());
    let name = h.dumps()[0]
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(name.ends_with(&format!("-none-{A}.dump")), "{name}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&h.backup_dir)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "backup dir mode {mode:o}");
}

#[test]
fn install_future_is_send() {
    fn assert_send<T: Send>(_: T) {}
    let h = H::new();
    let mut e = h.engine();
    assert_send(e.install(req(A)));
    assert_send(e.recover());
    assert_send(e.restore_previous(false));
}
