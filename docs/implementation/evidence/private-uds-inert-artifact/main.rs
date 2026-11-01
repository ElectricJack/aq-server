extern crate self as aq_playbook_host;
pub use aq_playbooks_model as model;
#[path = "development_definition.rs"]
mod definition;

fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&definition::artifact()).unwrap()
    );
}
