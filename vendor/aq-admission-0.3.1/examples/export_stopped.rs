fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&aq_admission::stopped::request_schema()).unwrap()
    );
}
