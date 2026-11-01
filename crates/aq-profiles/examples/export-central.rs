//! Normative data vectors only; no catalog activation, authority or runtime.
#[path = "../tests/central_common/mod.rs"]
mod common;
use aq_profiles::central::{Document, compile_central, schema};
fn main() {
    let output = std::path::PathBuf::from(std::env::args().nth(1).expect("output directory"));
    let (c, r) = common::fixture();
    let result = compile_central(&c, &r).expect("normative central fixture");
    let values = [
        (
            "catalog.example.json",
            serde_json::to_value(c.artifact()).unwrap(),
        ),
        (
            "request.example.json",
            serde_json::to_value(Document::Request(r)).unwrap(),
        ),
        (
            "compilation.example.json",
            serde_json::to_value(Document::Compilation(Box::new(result))).unwrap(),
        ),
        ("schema.json", schema()),
    ];
    std::fs::create_dir_all(&output).unwrap();
    for (name, value) in values {
        let bytes = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
        let path = output.join(name);
        if std::env::args().any(|arg| arg == "--check") {
            assert_eq!(
                std::fs::read(path).unwrap(),
                bytes.as_bytes(),
                "central normative artifact drift: {name}"
            );
        } else {
            std::fs::write(path, bytes).unwrap();
        }
    }
}
