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

/// Migration 0136: occupancy is the recorded compute release. Both startup guards accept
/// exactly the migrated functions, with no per-function settings (a hand-applied
/// `ALTER FUNCTION ... SET` once kept the server from starting) and no other body.
#[tokio::test]
async fn recorded_compute_release_136_upgrades_from_135_and_startup_refuses_drift() {
    use aq_server::{
        store::Store,
        test_support::{TestDb, migrate_profile},
    };
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 135).await.unwrap();
    let languages = "SELECT string_agg(l.lanname,',' ORDER BY p.proname) FROM pg_proc p JOIN pg_language l ON l.oid=p.prolang WHERE p.proname IN ('native_attempt_holds_compute','native_compute_reuse_proven')";
    let before: String = sqlx::query_scalar(languages)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, "plpgsql,sql");
    migrate_profile(&pool, 136).await.unwrap();
    let format: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(format, "136");
    let after: String = sqlx::query_scalar(languages)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        after, "plpgsql,plpgsql",
        "no SQL-language proof is planned per call"
    );
    // The occupancy read never reaches the stop proof: its plan names only the two tables.
    let source: String = sqlx::query_scalar(
        "SELECT prosrc FROM pg_proc WHERE proname='native_attempt_holds_compute'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(source.contains("execution_attempt_compute_releases"));
    assert!(!source.contains("profiled_compute_stopped_valid"));
    // Nothing but the validating owner writes a release.
    let direct = sqlx::query("INSERT INTO execution_attempt_compute_releases(attempt_id,node_id,proof,stop_receipt_digest,witnesses) VALUES(gen_random_uuid(),gen_random_uuid(),'stopped',repeat('a',64),'[]')")
        .execute(&pool)
        .await;
    assert!(direct.is_err());
    // A fresh start on the migrated database passes both guards; each drift is refused.
    Store::connect_with(db.options()).await.unwrap();
    for drift in [
        "ALTER FUNCTION native_attempt_holds_compute(uuid,uuid) SET join_collapse_limit=1",
        "ALTER FUNCTION native_compute_reuse_proven(uuid,uuid) SET from_collapse_limit=1",
        "CREATE OR REPLACE FUNCTION native_compute_release_record(attempt UUID) RETURNS BOOLEAN LANGUAGE plpgsql AS 'BEGIN RETURN TRUE; END'",
        "ALTER TABLE profiled_authority_hot DISABLE TRIGGER native_compute_release_on_stop",
        "ALTER TABLE execution_attempt_compute_releases DISABLE TRIGGER execution_attempt_compute_release_guard",
    ] {
        let db = TestDb::create().await;
        let pool = db.pool().await;
        migrate_profile(&pool, 136).await.unwrap();
        sqlx::raw_sql(drift).execute(&pool).await.unwrap();
        assert!(
            Store::connect_with(db.options()).await.is_err(),
            "startup accepted: {drift}"
        );
    }
}
