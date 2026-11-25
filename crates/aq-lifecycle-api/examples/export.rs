mod support;
use aq_lifecycle_api::*;
use std::{
    env, fs,
    path::{Path, PathBuf},
};
fn main() {
    let check = env::args().any(|x| x == "--check");
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let outputs = [
        ("request.schema.json", request_schema()),
        ("response.schema.json", response_schema()),
        ("fixtures.json", support::fixtures()),
    ];
    for (name, value) in outputs {
        let content = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
        let mut paths = vec![base.join("artifacts").join(name)];
        let root = base.join("../..");
        if root.join("protocol").is_dir() {
            paths.push(root.join("protocol/lifecycle-api/v1").join(name));
        }
        for path in paths {
            export(&path, &content, check);
        }
    }
}

fn export(path: &Path, content: &str, check: bool) {
    if check {
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            content,
            "{} drift",
            path.display()
        );
    } else {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}
