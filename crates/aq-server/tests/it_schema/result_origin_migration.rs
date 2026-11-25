use sqlx::postgres::PgPoolOptions;
#[path = "archive_receipts.rs"]
mod archive_receipts;

use aq_server::test_support::migrate_profile;
#[path = "../support/retained_archive.rs"]
mod retained_archive;
use retained_archive::{archive_snapshot, write_archive_evidence};
// Explicit retained-fixture qualification only. The archive is restored by the
// operator into a newly owned database before this test; no configured service
// or original fixture database can be selected through this entry point.
#[tokio::test]
#[ignore = "requires explicitly restored private actual populated36/37 archive clone"]
async fn selected_populated_archive37_to38_preserves_old_columns_and_receipts() {
    let (pool, directory, name, version) = selected_archive().await;
    let report_count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports")
        .fetch_one(&pool)
        .await
        .unwrap();
    let media_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM result_media_intents WHERE state='retained'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        report_count > 0,
        "must reuse an actual populated result owner"
    );
    assert_eq!(
        media_count, 2,
        "must reuse both actual retained PNG receipts"
    );
    // The retained archive is36. Use the real migration+guard implementation;
    // never stamp a format or skip the independently qualified37 guard.
    migrate_profile(&pool, 37).await.unwrap();
    let columns = archive_receipts::columns(&pool).await;
    let before = archive_snapshot(&pool, &columns, 37).await;
    let report_bytes = archive_receipts::report_receipts(&pool).await;
    let media_bytes = archive_receipts::media_receipts(&pool).await;
    write_archive_evidence(&directory.join("before37.json"), &before);
    migrate_profile(&pool, 38).await.unwrap();
    let after = archive_snapshot(&pool, &columns, 37).await;
    write_archive_evidence(&directory.join("after38.json"), &after);
    assert_eq!(
        before, after,
        "old column bags/schema/sequence states changed"
    );
    let after_reports = archive_receipts::report_receipts(&pool).await;
    let after_media = archive_receipts::media_receipts(&pool).await;
    assert!(
        report_bytes == after_reports,
        "actual report/Recorded receipt bytes changed"
    );
    assert!(
        media_bytes == after_media,
        "actual retained PNG receipt bytes changed"
    );
    let legacy: bool=sqlx::query_scalar("SELECT count(*)=$1 AND bool_and(result_origin_kind='legacy_prepare_v1' AND origin_execution_receipt_digest IS NULL AND origin_stopped_ack_bytes_digest IS NULL AND result_origin_phase_attempt_id IS NULL) FROM profiled_result_reports")
        .bind(report_count).fetch_one(&pool).await.unwrap();
    assert!(
        legacy,
        "every original result must acquire only explicit legacy origin"
    );
    let versions: Vec<i32> =
        sqlx::query_scalar("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(versions, (1..=38).collect::<Vec<_>>());
    let actual_format: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(actual_format, "38");
    let summary = serde_json::json!({"owned_clone":name,"entry_clone_format":version,"actual_migration_path":"36/37→37→38","old_tables":columns.len(),"original_reports":report_count,"exact_media_receipts":media_count,"old_columns_bags_schema_sequences_exact":true,"report_and_recorded_receipt_bytes_exact":true,"png_receipt_bytes_exact":true,"only_default_legacy_origin":true,"no_runtime_media_owner_or_filesystem_activation":true});
    write_archive_evidence(&directory.join("proof.json"), &summary);
    println!(
        "populated result38 proof old_tables={} original_reports={report_count} exact_media_receipts={media_count}",
        columns.len()
    );
    pool.close().await;
}

async fn selected_archive() -> (sqlx::PgPool, std::path::PathBuf, String, String) {
    let url = std::env::var("AQ_TEST_RESULT38_REHEARSAL_DATABASE_URL")
        .expect("explicit newly owned archive-clone URL required");
    let directory = std::path::PathBuf::from(
        std::env::var("AQ_TEST_RESULT38_REHEARSAL_EVIDENCE")
            .expect("explicit private new proof directory required"),
    );
    let metadata = std::fs::symlink_metadata(&directory).unwrap();
    use std::os::unix::fs::PermissionsExt;
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
        .strip_prefix("aq_result38_rehearsal_")
        .expect("only a newly named result38 rehearsal clone is permitted");
    assert!(uuid::Uuid::parse_str(suffix).is_ok());
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(matches!(version.as_str(), "36" | "37"));
    (pool, directory, name, version)
}
