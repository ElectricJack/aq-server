//! Generate/check the production dashboard action union and receipt schema.
fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/dashboard-actions/v1/schema.json");
    let bytes = aq_server::dashboard_api::actions::contract::schema_bytes();
    if std::env::args().any(|s| s == "--check") {
        assert_eq!(
            std::fs::read(path).unwrap(),
            bytes,
            "dashboard actions schema drift"
        );
    } else {
        std::fs::write(path, bytes).unwrap();
    }
}
