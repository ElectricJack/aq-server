use aq_playbook_host::protected::read_keyring;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
#[test]
fn private_key_file_rejects_public_symlink_malformed_oversized_and_missing_active() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("keys.json");
    let valid = format!(
        r#"{{"active_key_id":"one","keys":{{"one":"{}"}}}}"#,
        "ab".repeat(32)
    );
    fs::write(&path, &valid).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    read_keyring(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    assert!(read_keyring(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = root.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(read_keyring(&link).is_err());
    for bytes in [
        "x".repeat(4097),
        "{}".into(),
        valid.replace("\"one\",\"keys\"", "\"missing\",\"keys\""),
    ] {
        fs::write(&path, bytes).unwrap();
        assert!(read_keyring(&path).is_err());
    }
}
