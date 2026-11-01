fn main() {
    println!(
        "{}",
        String::from_utf8(aq_server::playbooks::inspection::definitions::schema_bytes()).unwrap()
    );
}
