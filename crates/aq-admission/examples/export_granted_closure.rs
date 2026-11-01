#[path = "../tests/granted_closure_common.rs"]
mod common;
fn main() {
    let value = match std::env::args().nth(1).as_deref() {
        Some("schema") => {
            serde_json::json!({"request":aq_admission::granted_closure::request_schema(),"receipt":aq_admission::granted_closure::receipt_schema()})
        }
        Some("fixtures") | None => common::fixtures(),
        _ => panic!("expected schema or fixtures"),
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
