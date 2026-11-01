#[path = "support/development_definition.rs"]
mod definition;

fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&definition::artifact()).unwrap()
    );
}
