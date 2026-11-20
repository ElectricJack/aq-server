//! D1 delivery owner for D3's default completion policy (`delivery =
//! required`). After D3 has accepted a result and holds only on missing
//! delivery evidence, this owner publishes the report's server-confirmed
//! retained source through the project's D1b publication policy, records the
//! server-owned delivery evidence D3 reads, and advances the project's source
//! head. Every aq-git call goes through D1a's durable intents and receipts; a
//! dispatched publication is never sent again.
//!
//! Attempts with a Git grant publish through the delivery playbook.
use super::owner::*;
use serde_json::Value;
use uuid::Uuid;
mod automatic_publish;
pub use automatic_publish::{awaits_delivery, deliver_then_settle};

pub use aq_delivery::source_checks::SourceHead;

/// The current recorded source head, if AQ has published to this target.
pub async fn current_head<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    project: Uuid,
    repository: Uuid,
    generation: u64,
    target: &str,
) -> Result<Option<SourceHead>> {
    let row: Option<Value> = sqlx::query_scalar(
        "SELECT evidence->'source_head' FROM profiled_delivery_evidence \
         WHERE state='published' AND evidence->'source_head'->>'project_id'=$1 \
         AND evidence->'source_head'->>'repository_id'=$2 \
         AND evidence->'source_head'->>'binding_generation'=$3 \
         AND evidence->'source_head'->>'target'=$4 \
         ORDER BY (evidence->'source_head'->>'sequence')::bigint DESC LIMIT 1",
    )
    .bind(project.to_string())
    .bind(repository.to_string())
    .bind(generation.to_string())
    .bind(target)
    .fetch_optional(executor)
    .await
    .map_err(db)?;
    let Some(value) = row else { return Ok(None) };
    let head: SourceHead = serde_json::from_value(value).map_err(|_| conflict())?;
    if !head.validate() {
        return Err(conflict());
    }
    Ok(Some(head))
}
