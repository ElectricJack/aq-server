use aq_server::test_support::migrate_profile;
// Explicit retained-fixture qualification only. The archive is restored by the
// operator into a newly owned database before this test; no configured service
// or original fixture database can be selected through this entry point.
#[tokio::test]
#[ignore = "requires explicitly restored private actual populated36/37 archive clone"]
async fn selected_populated_archive37_to38_preserves_old_columns_and_receipts() {
    use sqlx::postgres::PgPoolOptions;
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
    let columns: std::collections::BTreeMap<String, Vec<String>> = {
        let rows: Vec<(String, Vec<String>)> = sqlx::query_as("SELECT c.relname,array_agg(a.attname::text ORDER BY a.attnum) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_attribute a ON a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped WHERE n.nspname='public' AND c.relkind IN('r','p') GROUP BY c.relname ORDER BY c.relname")
            .fetch_all(&pool).await.unwrap();
        rows.into_iter().collect()
    };
    let before = archive_snapshot(&pool, &columns).await;
    let report_bytes: Vec<(uuid::Uuid, Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT report_id,report_bytes,receipt_bytes FROM profiled_result_reports ORDER BY report_id")
            .fetch_all(&pool).await.unwrap();
    let media_bytes: Vec<(uuid::Uuid, Vec<u8>)> = sqlx::query_as("SELECT operation_id,receipt_bytes FROM result_media_intents WHERE state='retained' ORDER BY operation_id")
        .fetch_all(&pool).await.unwrap();
    write_archive_evidence(&directory.join("before37.json"), &before);
    migrate_profile(&pool, 38).await.unwrap();
    let after = archive_snapshot(&pool, &columns).await;
    write_archive_evidence(&directory.join("after38.json"), &after);
    assert_eq!(
        before, after,
        "old column bags/schema/sequence states changed"
    );
    let after_reports: Vec<(uuid::Uuid, Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT report_id,report_bytes,receipt_bytes FROM profiled_result_reports ORDER BY report_id")
            .fetch_all(&pool).await.unwrap();
    let after_media: Vec<(uuid::Uuid, Vec<u8>)> = sqlx::query_as("SELECT operation_id,receipt_bytes FROM result_media_intents WHERE state='retained' ORDER BY operation_id")
        .fetch_all(&pool).await.unwrap();
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

fn archive_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn write_archive_evidence(path: &std::path::Path, value: &serde_json::Value) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec_pretty(value).unwrap())
        .unwrap();
    file.sync_all().unwrap();
}
async fn archive_snapshot(
    pool: &sqlx::PgPool,
    columns: &std::collections::BTreeMap<String, Vec<String>>,
) -> serde_json::Value {
    use serde_json::{Value, json};
    use sqlx::Row;
    let mut tables = serde_json::Map::new();
    let mut column_definitions = serde_json::Map::new();
    for (table, names) in columns {
        let projection = names
            .iter()
            .map(|v| format!("t.{}", archive_ident(v)))
            .collect::<Vec<_>>()
            .join(",");
        let predicate = match table.as_str() {
            "meta" => " WHERE key<>'state_format_version'",
            "schema_migrations" => " WHERE version<=37",
            _ => "",
        };
        let query = format!(
            "SELECT jsonb_build_object('count',count(*),'sha256',encode(sha256(convert_to(COALESCE(string_agg(h,'' ORDER BY h),''),'UTF8')),'hex')) FROM (SELECT encode(sha256(convert_to(to_jsonb(t)::text,'UTF8')),'hex') h FROM (SELECT {projection} FROM public.{} t{predicate}) t) bag",
            archive_ident(table)
        );
        let bag: Value = sqlx::query_scalar(&query).fetch_one(pool).await.unwrap();
        tables.insert(table.clone(), bag);
        let defs:Value=sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_object('name',a.attname,'type',format_type(a.atttypid,a.atttypmod),'notnull',a.attnotnull,'identity',a.attidentity,'generated',a.attgenerated,'default',pg_get_expr(d.adbin,d.adrelid)) ORDER BY a.attnum) FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE n.nspname='public' AND c.relname=$1 AND a.attname=ANY($2) AND NOT a.attisdropped")
            .bind(table).bind(names).fetch_one(pool).await.unwrap();
        column_definitions.insert(table.clone(), defs);
    }
    let sequence_names:Vec<String>=sqlx::query_scalar("SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind='S' ORDER BY c.relname").fetch_all(pool).await.unwrap();
    let mut sequences = serde_json::Map::new();
    for name in sequence_names {
        let row = sqlx::query(&format!(
            "SELECT last_value,is_called FROM public.{}",
            archive_ident(&name)
        ))
        .fetch_one(pool)
        .await
        .unwrap();
        sequences.insert(name,json!({"last_value":row.get::<i64,_>("last_value"),"is_called":row.get::<bool,_>("is_called")}));
    }
    json!({"old_table_column_bags":tables,"old_column_definitions":column_definitions,"sequence_states":sequences})
}
