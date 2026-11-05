#[path = "../tests/started_common.rs"]
mod started_common;
fn main() {
    let value = if std::env::args().nth(1).as_deref() == Some("schema") {
        aq_admission::started::request_schema()
    } else {
        serde_json::to_value(started_common::request()).unwrap()
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
