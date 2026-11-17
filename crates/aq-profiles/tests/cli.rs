//! `aq-profile validate <directory>` on the shipped defaults and on a broken set.
use std::process::Command;

fn validate(directory: &std::path::Path) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_aq-profile"))
        .arg("validate")
        .arg(directory)
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

#[test]
fn the_shipped_defaults_are_a_valid_set() {
    let defaults = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("defaults");
    let (code, out) = validate(&defaults);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.starts_with("valid: 12 documents, snapshot "), "{out}");
}

#[test]
fn every_problem_is_printed_with_its_document() {
    let dir = tempfile::tempdir().unwrap();
    let roles = dir.path().join("roles");
    std::fs::create_dir(&roles).unwrap();
    // A role whose execution profile does not exist, and no level at all.
    std::fs::write(
        roles.join("developer.md"),
        "---\nid: developer\nname: Developer\ndescription: Makes changes.\nexecution: absent\noperations: [task.read]\nenabled: true\n---\n## Role\nYou make the change.\n",
    )
    .unwrap();
    let (code, out) = validate(dir.path());
    assert_eq!(code, Some(3), "{out}");
    assert!(out.contains("developer"), "{out}");
    assert!(out.contains("absent"), "{out}");
}
