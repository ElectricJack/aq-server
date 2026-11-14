//! Generate the canonical dashboard-read schema from the contract crate.
fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/dashboard-read/v1/schema.json");
    let bytes = aq_dashboard_contract::schema_bytes();
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
