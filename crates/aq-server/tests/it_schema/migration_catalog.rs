#[path = "../../migration_catalog.rs"]
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
    for name in [
        "1_a.sql",
        "0000_a.sql",
        "0001_.sql",
        "0001_B.sql",
        "0001_é.sql",
        "０００１_a.sql",
        "0001_a.sql.sql",
        "0001_a-b.sql",
        "0001_a.SQL",
    ] {
        assert!(catalog::validate(&paths(&[name])).is_err(), "{name}");
    }
}

#[test]
fn canonical_names_preserve_generated_source_bytes() {
    let entries = catalog::validate(&paths(&["0001_a_2.sql", "0002__.sql"])).unwrap();
    assert_eq!(
        catalog::rust_source(&entries),
        concat!(
            "pub(crate) const MIGRATIONS: &[(i32, &str)] = &[\n",
            "    (1, include_str!(\"0001_a_2.sql\")),\n",
            "    (2, include_str!(\"0002__.sql\")),\n",
            "];\n",
            "pub const STATE_FORMAT_VERSION: u32 = 2;\n",
            "pub const READABLE_STATE_FORMATS: &[u32] = &[1,2,];\n",
        )
    );
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

#[tokio::test]
async fn node_slots_135_preserves_existing_counts_at_one_and_defaults_new_nodes_to_two() {
    use aq_server::test_support::{TestDb, migrate_profile};
    use uuid::Uuid;
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 134).await.unwrap();
    let existing = Uuid::new_v4();
    let insert = "INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'node',$2,'test',2,now())";
    sqlx::query(insert)
        .bind(existing)
        .bind(existing.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO worker_settings(kind,id,max_agents,revision) VALUES('node',$1,8,1)")
        .bind(existing.to_string())
        .execute(&pool)
        .await
        .unwrap();
    migrate_profile(&pool, 135).await.unwrap();
    let new = Uuid::new_v4();
    sqlx::query(insert)
        .bind(new)
        .bind(new.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        aq_machine_control::slots::count(&mut conn, existing)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        aq_machine_control::slots::count(&mut conn, new)
            .await
            .unwrap(),
        2
    );
}
