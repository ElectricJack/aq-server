//! Normative structural vectors only; no owner authentication or runtime.
#[path = "../tests/preparation_refresh_common.rs"]
mod fixture;
use aq_admission::preparation_refresh_v1;
fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).expect("output directory"));
    let check = std::env::args().any(|arg| arg == "--check");
    let values = [
        ("schema.json", preparation_refresh_v1::schema()),
        (
            "contract.json",
            serde_json::json!({"artifact":preparation_refresh_v1::contract_artifact().unwrap(),"manifest":preparation_refresh_v1::contract_manifest().unwrap()}),
        ),
        ("examples.json", fixture::fixtures()),
    ];
    if !check {
        std::fs::create_dir_all(&out).unwrap();
    }
    for (name, value) in values {
        let bytes = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
        if check {
            assert_eq!(
                std::fs::read(out.join(name)).unwrap(),
                bytes.as_bytes(),
                "refresh public vector drift: {name}"
            );
        } else {
            std::fs::write(out.join(name), bytes).unwrap();
        }
    }
}
