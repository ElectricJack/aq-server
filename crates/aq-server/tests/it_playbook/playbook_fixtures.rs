//! Shared mechanics for synthetic playbook fixtures; policy stays with callers.
use aq_server::protected_storage::{KeyFile, Keyring, ProtectionConfig};
use axum::{Router, body::Body, http::Request, response::Response};
use std::os::unix::fs::PermissionsExt;
use tower::ServiceExt;

pub(super) fn keyring() -> Keyring {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("key");
    std::fs::write(&path, [9_u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    // Keyring owns the loaded bytes; the private file can disappear on return.
    Keyring::load(&ProtectionConfig {
        active_key_id: "test".into(),
        keys: vec![KeyFile {
            id: "test".into(),
            file: path,
        }],
    })
    .unwrap()
}

pub(super) async fn dev_get(app: &Router, email: &str, path: &str) -> Response {
    app.clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", email)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[test]
fn loaded_keys_survive_private_file_cleanup_and_reopen_retained_bytes() {
    let aad = b"playbook fixture record";
    let sealed = keyring().seal(aad, b"retained fixture bytes").unwrap();
    let reopened = keyring();
    assert_eq!(
        reopened.open(aad, &sealed).unwrap().as_slice(),
        b"retained fixture bytes"
    );
    assert!(reopened.open(b"another fixture record", &sealed).is_err());
}
