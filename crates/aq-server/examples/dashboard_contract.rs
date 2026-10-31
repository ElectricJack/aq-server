//! Generate/check the implemented dashboard-read-1 schema without a runtime.
use aq_server::dashboard_api::contract;
fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/dashboard-read/v1/schema.json");
    let bytes = contract::schema_bytes();
    if std::env::args().any(|s| s == "--check") {
        assert_eq!(
            std::fs::read(path).unwrap(),
            bytes,
            "dashboard schema drift"
        );
    } else {
        std::fs::write(path, bytes).unwrap();
    }
}
