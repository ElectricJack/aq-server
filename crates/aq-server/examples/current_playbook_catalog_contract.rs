//! Closed human-read DTO schema; no runtime/database access.
fn main() {
    use std::io::Write;
    std::io::stdout()
        .write_all(&aq_server::playbooks::inspection::catalog::schema_bytes())
        .unwrap();
}
