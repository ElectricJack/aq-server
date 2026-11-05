//! Export/check the exact Results feedback payload and projection schemas.
fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/results-feedback/v1/schema.json");
    let bytes = aq_server::results_feedback::contract::schema_bytes();
    if std::env::args().any(|a| a == "--check") {
        assert_eq!(std::fs::read(path).unwrap(), bytes, "Results schema drift");
    } else {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}
