#[path = "../tests/preparation_v2_common.rs"]
mod common;
fn main() {
    let value = match std::env::args().nth(1).as_deref() {
        Some("schema") => aq_admission::preparation_v2::schema(),
        Some("contract") => {
            serde_json::json!({"artifact":aq_admission::preparation_v2::contract_artifact().unwrap(),"manifest":aq_admission::preparation_v2::contract_manifest().unwrap()})
        }
        Some("fixtures") | None => common::fixtures(),
        _ => panic!("expected schema,contract,fixtures"),
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
