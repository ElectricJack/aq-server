//! Explicit populated archive qualification; no configured application database.
#[path = "archive_receipts.rs"]
mod archive_receipts;

#[path = "../support/retained_archive.rs"]
mod retained_archive;

use aq_server::test_support::migrate_profile;
use retained_archive::{archive_snapshot, write_archive_evidence};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
#[ignore = "requires an explicitly restored populated archive in a fresh settlement39 rehearsal clone"]
async fn populated_archive38_to39_preserves_legacy_rows_and_receipts() {
    let (pool, directory, name, entry) = selected_archive().await;
    // Use the actual registered migration/attestation owner throughout; never
    // change format metadata or bypass result38 before measuring its old data.
    migrate_profile(&pool, 38).await.unwrap();
    let reports = archive_receipts::report_receipts(&pool).await;
    let media = archive_receipts::media_receipts(&pool).await;
    assert!(
        !reports.is_empty(),
        "actual populated legacy result required"
    );
    assert_eq!(media.len(), 2, "both actual retained PNG receipts required");
    let columns = archive_receipts::columns(&pool).await;
    let before = archive_snapshot(&pool, &columns, 38).await;
    write_archive_evidence(&directory.join("before38.json"), &before);
    migrate_profile(&pool, 39).await.unwrap();
    // Reopening the real owner must attest39 without learning/repairing it.
    migrate_profile(&pool, 39).await.unwrap();
    let after = archive_snapshot(&pool, &columns, 38).await;
    write_archive_evidence(&directory.join("after39.json"), &after);
    assert_eq!(
        before, after,
        "old table column bags/schema/sequence state changed"
    );
    let after_reports = archive_receipts::report_receipts(&pool).await;
    let after_media = archive_receipts::media_receipts(&pool).await;
    assert_eq!(
        reports, after_reports,
        "legacy report/Recorded receipt bytes changed"
    );
    assert_eq!(media, after_media, "retained PNG receipt bytes changed");
    let legacy_profiles: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM review_subjects WHERE subject_profile<>'code_review_v1') AND NOT EXISTS(SELECT 1 FROM human_decision_finality WHERE combined_actor_digest IS NOT NULL OR combined_actor_revision IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM profiled_verified_releases WHERE settlement_operation_id IS NOT NULL)"
    ).fetch_one(&pool).await.unwrap();
    assert!(
        legacy_profiles,
        "historical data must acquire only legacy/null owner fields"
    );
    let fresh: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM profiled_settlement_evaluations),(SELECT count(*) FROM profiled_task_completions)"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(
        fresh,
        (0, 0),
        "upgrade must not fabricate settlement/completion"
    );
    let versions: Vec<i32> =
        sqlx::query_scalar("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(versions, (1..=39).collect::<Vec<_>>());
    let format: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(format, "39");
    write_archive_evidence(
        &directory.join("proof.json"),
        &serde_json::json!({
            "owned_clone":name, "entry_format":entry,
            "actual_migration_path":"36/37/38→38→39→39 attestation",
            "old_tables":columns.len(), "original_reports":reports.len(),
            "exact_media_receipts":media.len(), "old_columns_bags_schema_sequences_exact":true,
            "report_and_recorded_receipt_bytes_exact":true, "png_receipt_bytes_exact":true,
            "only_legacy_or_null_new_fields":true, "no_fabricated_settlement_or_completion":true,
            "no_runtime_media_owner_or_filesystem_activation":true
        }),
    );
    println!(
        "populated settlement39 proof old_tables={} reports={} retained_png_receipts={}",
        columns.len(),
        reports.len(),
        media.len()
    );
    pool.close().await;
}

async fn selected_archive() -> (sqlx::PgPool, std::path::PathBuf, String, String) {
    let url = std::env::var("AQ_TEST_SETTLEMENT39_REHEARSAL_DATABASE_URL")
        .expect("explicit newly owned archive-clone URL required");
    let directory = std::path::PathBuf::from(
        std::env::var("AQ_TEST_SETTLEMENT39_REHEARSAL_EVIDENCE")
            .expect("explicit private new proof directory required"),
    );
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::symlink_metadata(&directory).unwrap();
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let suffix = name
        .strip_prefix("aq_settlement39_rehearsal_")
        .expect("only a newly named settlement39 archive clone is permitted");
    assert!(uuid::Uuid::parse_str(suffix).is_ok());
    let entry: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(matches!(entry.as_str(), "36" | "37" | "38"));
    (pool, directory, name, entry)
}
