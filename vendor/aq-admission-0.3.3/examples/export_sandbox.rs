fn main() {
    let schema = serde_json::to_string_pretty(&aq_admission::sandbox::schema()).unwrap() + "\n";
    if let Some(path) = std::env::args_os().nth(1) {
        std::fs::write(path, schema).unwrap();
    } else {
        print!("{schema}");
    }
}
