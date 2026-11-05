use aq_responses_transport::RetainedUpstreamCredential;
use aq_supervisor_host::credentials::SecretFile;
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn credential_reader_refuses_unsafe_files_and_rotation_without_disclosure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("provider");
    std::fs::write(&path, b"test-provider-secret").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let source = SecretFile::open(path.clone()).unwrap();
    assert_eq!(&*source.read_current().unwrap(), b"test-provider-secret");
    assert!(!format!("{source:?}").contains("test-provider-secret"));
    std::fs::write(&path, b"rotated-provider-secret").unwrap();
    let error = source.read_current().unwrap_err().to_string();
    assert!(!error.contains("rotated-provider-secret"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(SecretFile::open(path.clone()).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(SecretFile::open(link).is_err());
    std::fs::write(&path, vec![b'a'; 4097]).unwrap();
    assert!(SecretFile::open(path).is_err());
}

#[tokio::test]
async fn supervisor_staging_is_private_bounded_and_rejects_unsafe_sources() {
    use aq_supervisor_host::credentials::stage_supervisor_credential;
    let launch = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let source = launch.path().join("supervisor.credential");
    let token = format!("aqsv1_{}", "a".repeat(64));
    std::fs::write(&source, &token).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
    let staged = stage_supervisor_credential(&source, home.path(), u64::MAX)
        .await
        .unwrap();
    assert_eq!(staged, home.path().join("supervisor.credential"));
    assert_eq!(std::fs::read_to_string(&staged).unwrap(), token);
    assert_eq!(
        std::fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), token);
    assert!(
        stage_supervisor_credential(&source, home.path(), u64::MAX)
            .await
            .is_err(),
        "no overwrite"
    );
    let other = tempfile::tempdir().unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        stage_supervisor_credential(&source, other.path(), u64::MAX)
            .await
            .is_err()
    );
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = launch.path().join("link");
    symlink(&source, &link).unwrap();
    assert!(
        stage_supervisor_credential(&link, other.path(), u64::MAX)
            .await
            .is_err()
    );
    assert!(
        stage_supervisor_credential(&source, other.path(), 0)
            .await
            .is_err(),
        "expired before staging"
    );
    assert!(
        stage_supervisor_credential(&launch.path().join("missing"), other.path(), 0)
            .await
            .is_err()
    );
    assert!(!other.path().join("supervisor.credential").exists());
}

#[tokio::test(start_paused = true)]
async fn supervisor_staging_waits_for_post_running_installation() {
    use aq_supervisor_host::credentials::stage_supervisor_credential;
    let launch = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let source = launch.path().join("supervisor.credential");
    let source_for_wrapper = source.clone();
    let native_home = home.path().to_owned();
    let waiting = tokio::spawn(async move {
        stage_supervisor_credential(&source_for_wrapper, &native_home, u64::MAX).await
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished(), "native waits for the controller");
    std::fs::write(&source, []).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
    tokio::time::advance(std::time::Duration::from_millis(25)).await;
    tokio::task::yield_now().await;
    assert!(
        !waiting.is_finished(),
        "native waits through create-before-write"
    );
    let token = format!("aqsv1_{}", "b".repeat(64));
    std::fs::write(&source, &token).unwrap();
    tokio::time::advance(std::time::Duration::from_millis(25)).await;
    let staged = waiting.await.unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(staged).unwrap(), token);
}
