fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&aq_admission::result_record::receipt_schema()).unwrap()
    );
}
