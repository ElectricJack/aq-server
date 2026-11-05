#[path = "../migration_catalog.rs"]
mod catalog;

use std::path::PathBuf;

fn paths(names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(PathBuf::from).collect()
}

#[test]
fn rejects_duplicate_missing_and_out_of_order_numbers() {
    for names in [
        vec!["0001_a.sql", "0001_b.sql"],
        vec!["0001_a.sql", "0003_c.sql"],
        vec!["0002_b.sql", "0001_a.sql"],
    ] {
        let error = catalog::validate(&paths(&names)).unwrap_err();
        assert!(error.contains("duplicate, missing or out-of-order"));
    }
}

#[test]
fn rejects_empty_and_noncanonical_catalogs() {
    assert!(catalog::validate(&[]).is_err());
    for name in ["1_a.sql", "0000_a.sql", "0001_.sql", "0001_B.sql"] {
        assert!(catalog::validate(&paths(&[name])).is_err(), "{name}");
    }
}

#[test]
fn directory_additions_generate_registration_and_state_formats() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("0001_a.sql");
    std::fs::write(&first, "-- first").unwrap();
    let entries = catalog::discover(directory.path()).unwrap();
    assert_eq!(entries.len(), 1);
    // No registration edits: adding a file alone changes all derived metadata.
    let second = directory.path().join("0002_b.sql");
    std::fs::write(&second, "-- second").unwrap();
    let entries = catalog::discover(directory.path()).unwrap();
    assert_eq!(entries, vec![(1, first), (2, second.clone())]);
    let generated = catalog::rust_source(&entries);
    assert!(generated.contains(&format!(
        "(2, include_str!({:?}))",
        second.to_str().unwrap()
    )));
    assert!(generated.contains("STATE_FORMAT_VERSION: u32 = 2;"));
    assert!(generated.contains("READABLE_STATE_FORMATS: &[u32] = &[1,2,];"));
    std::fs::write(directory.path().join("0002_duplicate.sql"), "-- duplicate").unwrap();
    assert!(catalog::discover(directory.path()).is_err());
}
