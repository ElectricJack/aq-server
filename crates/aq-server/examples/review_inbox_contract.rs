//! Generate the standalone schema for the actual human review inbox DTO.
fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&aq_server::reviews::inbox_contract_schema()).unwrap()
    );
}
