use serde_json::Value;

pub(super) fn assert_pinned_files(pin: &Value, fingerprint: &str, directory: &str) {
    assert_eq!(pin["catalog_fingerprint"], fingerprint);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor")
        .join(directory);
    let files = pin["files"].as_array().unwrap();
    assert_eq!(files.len(), std::fs::read_dir(&root).unwrap().count());
    for file in files {
        let bytes = std::fs::read(root.join(file["path"].as_str().unwrap())).unwrap();
        assert_eq!(file["bytes"], bytes.len());
        assert_eq!(
            file["sha256"].as_str().unwrap(),
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
        );
    }
}
