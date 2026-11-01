fn main() {
    println!(
        "{}",
        String::from_utf8(aq_server::playbooks::inspection::schema_bytes()).unwrap()
    );
}
