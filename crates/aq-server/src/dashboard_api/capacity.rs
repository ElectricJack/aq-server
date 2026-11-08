//! X1/X2 capacity read: slot load and placement eligibility per worker, and
//! provider-account availability from unexpired node credential reports.
//! Ceilings come from the placement owner, so a refusal reason here is the
//! reason placement would give now. Owner-only; nothing here grants placement.
use super::{
    contract::*,
    runtime::Runtime,
    service::{auth, envelope, fence, stamp, timestamp, tx},
    *,
};
use crate::{
    node_storage,
    profiled_execution::placement::{self, Ledger},
    store::User,
};
use aq_swarm::capacity::{self as swarm, AccountInput, NodeInput, Snapshot};
use sqlx::Row;
use std::collections::BTreeMap;

/// Nodes read per request; more marks the response partial.
const MAX_NODES: usize = 256;

pub(super) async fn capacity(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    scope: Scope,
) -> Result<Envelope<Capacity>> {
    let a = auth(state, runtime, user).await?;
    if !a.owner {
        return Err(AppError::Forbidden("capacity requires owner".into()).into());
    }
    let now = stamp(state)?;
    let now_at = (state.clock)();
    let offline_after = Duration::from_secs(state.config.workers.offline_after_secs);
    let mut tx = tx(runtime).await?;
    let rows = sqlx::query(
        "SELECT id,name,worker_version,protocol_version,enrolled_at,last_seen_at,revoked_at \
         FROM nodes ORDER BY name,id LIMIT $1",
    )
    .bind((MAX_NODES + 1) as i64)
    .fetch_all(&mut *tx)
    .await?;
    let truncated = rows.len() > MAX_NODES;
    let nodes: Vec<crate::store::Node> = rows
        .iter()
        .take(MAX_NODES)
        .map(|r| crate::store::Node {
            id: r.get("id"),
            name: r.get("name"),
            worker_version: r.get("worker_version"),
            protocol_version: r.get::<i32, _>("protocol_version") as u32,
            enrolled_at: r.get("enrolled_at"),
            last_seen_at: r.get("last_seen_at"),
            revoked_at: r.get("revoked_at"),
        })
        .collect();
    let ids: Vec<Uuid> = nodes.iter().map(|n| n.id).collect();
    let snapshots: BTreeMap<Uuid, Snapshot> = sqlx::query_as::<_, (Uuid, serde_json::Value)>(
        "SELECT node_id, snapshot FROM intelligence_node_snapshots WHERE node_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .filter_map(|(node, value)| Some((node, serde_json::from_value(value).ok()?)))
    .collect();
    let book = crate::observations::catalog::rate_book(state);
    let ledger = Ledger {
        pool: &state.store.observation_pool,
        book: &book,
    };
    let loads = placement::loads(&mut tx, &ledger, &ids, now).await?;
    let storage = node_storage::latest(&mut tx, &ids).await?;
    let occupied_all: i64 =
        sqlx::query_scalar("SELECT count(*) FROM execution_attempts WHERE holds_task")
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    let mut out = Vec::with_capacity(nodes.len());
    for (node, load) in nodes.iter().zip(&loads) {
        let health = swarm::health(crate::admin_api::worker_status(node, now_at, offline_after));
        let storage = storage
            .iter()
            .find(|(n, ..)| *n == node.id)
            .map(|(_, r, bps, at)| {
                Ok::<_, Error>(NodeStorage {
                    used_bps: u16::try_from(*bps).unwrap_or(10_000).min(10_000),
                    disk_total_bytes: r.disk_total_bytes,
                    disk_used_bytes: r.disk_used_bytes,
                    disk_free_bytes: r.disk_free_bytes,
                    disk_reserved_bytes: r.disk_reserved_bytes,
                    preserved_count: r.preserved_count,
                    preserved_bytes: r.preserved_bytes,
                    observed_at: timestamp(*at)?,
                })
            })
            .transpose()?;
        out.push(swarm::node_capacity(
            NodeInput {
                id: node.id,
                name: &node.name,
                health,
                last_seen_ms: node.last_seen_at.map(swarm::millis),
                backend_kind: load
                    .backend
                    .and_then(|k| serde_json::to_value(k).ok())
                    .and_then(|v| v.as_str().map(str::to_owned)),
                total: load.total,
                occupied: load.occupied,
                ceiling: load.ceiling,
                snapshot: snapshots.get(&node.id),
                storage,
            },
            now,
        )?);
    }

    let providers = providers(state, &nodes, &snapshots, now)?;
    let (slots, reason) = swarm::slot_summary(
        &out,
        u32::try_from(occupied_all).unwrap_or(u32::MAX),
        truncated,
        now,
    )?;
    fence(state, runtime, user, &a).await?;
    envelope(
        state,
        scope,
        Capacity {
            slots,
            nodes: out,
            providers,
        },
        reason.as_deref(),
    )
}

/// Per configured account: available while an unrevoked node holds a live binding.
fn providers(
    state: &AppState,
    nodes: &[crate::store::Node],
    snapshots: &BTreeMap<Uuid, Snapshot>,
    now: u64,
) -> Result<Vec<ProviderAvailability>> {
    let registry = state.config.intelligence.as_deref();
    let live = swarm::live(
        nodes.iter().filter(|n| n.revoked_at.is_none()),
        |n| snapshots.get(&n.id),
        now,
    );
    let accounts = crate::provider_accounts::project(registry, now)
        .accounts
        .into_iter()
        .map(|account| AccountInput {
            profiles: registry
                .map(|r| r.credentials.as_slice())
                .unwrap_or_default()
                .iter()
                .filter(|c| {
                    c.provider == account.provider
                        && c.account_reference == account.account_reference
                })
                .map(|c| c.profile.clone())
                .collect(),
            provider: account.provider,
            account_reference: account.account_reference,
            enabled: account.enabled,
        })
        .collect();
    Ok(swarm::provider_availability(accounts, &live, now)?)
}
