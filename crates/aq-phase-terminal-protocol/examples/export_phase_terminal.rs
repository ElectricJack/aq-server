#[path = "../tests/common.rs"]
mod common;
fn main() {
    let v = match std::env::args().nth(1).as_deref() {
        Some("invalid") => common::invalid_fixtures(),
        Some("schema") => aq_phase_terminal_protocol::schema(),
        Some("manifest") => aq_phase_terminal_protocol::contract_manifest().unwrap(),
        Some("artifact") => {
            serde_json::to_value(aq_phase_terminal_protocol::contract_artifact().unwrap()).unwrap()
        }
        _ => common::fixtures(),
    };
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
}
