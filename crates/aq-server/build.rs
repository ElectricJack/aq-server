mod migration_catalog;

fn main() {
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let directory = root.join("migrations");
    // Watch the directory as well as its contents: adding/removing a file must
    // rebuild the catalog even when none of the old files changed.
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=migration_catalog.rs");
    println!("cargo:rerun-if-env-changed=AQ_IMAGE_STATE_FORMAT");
    let entries = migration_catalog::discover(&directory).unwrap_or_else(|e| panic!("{e}"));
    let latest = entries.last().unwrap().0;
    // Docker labels must match this exact source tree, including local builds.
    if let Ok(label) = std::env::var("AQ_IMAGE_STATE_FORMAT") {
        assert_eq!(
            label,
            latest.to_string(),
            "AQ_STATE_FORMAT build argument must match the migration catalog"
        );
    }
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(
        output.join("migration_catalog.rs"),
        migration_catalog::rust_source(&entries),
    )
    .expect("write generated migration catalog");
}
