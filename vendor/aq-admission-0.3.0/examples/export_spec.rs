#[path = "../tests/spec_common.rs"]
mod fixture;
fn main() {
    let value = if std::env::args().nth(1).as_deref() == Some("schema") {
        aq_admission::execution_spec::execution_spec_schema()
    } else {
        let (spec, offer) = fixture::fixture();
        serde_json::json!({"spec":spec,"offer":offer})
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
