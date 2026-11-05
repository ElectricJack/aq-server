#[path = "../tests/startup_common.rs"]
mod fixture;
fn main() {
    let value = if std::env::args().nth(1).as_deref() == Some("schema") {
        aq_admission::startup::native_offer_schema()
    } else {
        let (offer, bytes) = fixture::fixture();
        serde_json::json!({"native_offer":offer,"payload_utf8":String::from_utf8(bytes).unwrap()})
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
