//! Read-only projections shared by the explicitly selected archive rehearsals.
use sqlx::PgPool;
use std::collections::BTreeMap;
use uuid::Uuid;

type ReportReceipt = (Uuid, Vec<u8>, Vec<u8>);
type MediaReceipt = (Uuid, Vec<u8>);

pub async fn columns(pool: &PgPool) -> BTreeMap<String, Vec<String>> {
    sqlx::query_as::<_, (String, Vec<String>)>(
        "SELECT c.relname,array_agg(a.attname::text ORDER BY a.attnum) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_attribute a ON a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped WHERE n.nspname='public' AND c.relkind IN('r','p') GROUP BY c.relname ORDER BY c.relname"
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .collect()
}

pub async fn report_receipts(pool: &PgPool) -> Vec<ReportReceipt> {
    sqlx::query_as(
        "SELECT report_id,report_bytes,receipt_bytes FROM profiled_result_reports ORDER BY report_id"
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

pub async fn media_receipts(pool: &PgPool) -> Vec<MediaReceipt> {
    sqlx::query_as(
        "SELECT operation_id,receipt_bytes FROM result_media_intents WHERE state='retained' ORDER BY operation_id"
    )
    .fetch_all(pool)
    .await
    .unwrap()
}
