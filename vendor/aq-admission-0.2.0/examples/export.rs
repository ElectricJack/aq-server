#[path = "../tests/common.rs"]
mod common;
fn main() {
    let what = std::env::args().nth(1).unwrap_or_default();
    let value = if what == "schema" {
        aq_admission::contract_schema()
    } else {
        serde_json::json!(common::documents())
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
