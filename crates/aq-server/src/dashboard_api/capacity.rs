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
use aq_selection::{ArtifactRef, NodeCeiling};
use serde::Deserialize;
use sqlx::Row;
use std::collections::BTreeMap;

/// Nodes read per request; more marks the response partial.
const MAX_NODES: usize = 256;

/// The capability-snapshot facts capacity needs. Unknown snapshot fields are
/// ignored: the owner validated the full snapshot when the node reported it.
#[derive(Deserialize)]
struct Snapshot {
    #[serde(default)]
    observed_at_ms: Option<u64>,
    expires_at_ms: u64,
    #[serde(default)]
    credential_bindings: Vec<Binding>,
}
#[derive(Deserialize)]
struct Binding {
    credential_profile: ArtifactRef,
    available: bool,
    observed_at_ms: u64,
    expires_at_ms: u64,
}

fn reason(ceiling: NodeCeiling) -> IneligibleReason {
    match ceiling {
        NodeCeiling::Concurrency => IneligibleReason::NodeConcurrencyCeilingReached,
        NodeCeiling::Spend => IneligibleReason::NodeSpendCeilingReached,
        NodeCeiling::SpendUnaccounted => IneligibleReason::NodeSpendUnaccounted,
        NodeCeiling::DiskPressure => IneligibleReason::NodeDiskPressure,
    }
}

fn exact(value: u32) -> Total {
    Total {
        value: Some(value),
        accuracy: Accuracy::Exact,
    }
}

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
    let (mut total, mut idle, mut unadvertised) = (0u32, 0u32, false);
    for (node, load) in nodes.iter().zip(&loads) {
        let health = match crate::admin_api::worker_status(node, now_at, offline_after) {
            "online" => WorkerHealth::Online,
            "revoked" => WorkerHealth::Revoked,
            _ => WorkerHealth::Offline,
        };
        let snapshot = snapshots.get(&node.id);
        let mut reasons = Vec::new();
        match health {
            WorkerHealth::Revoked => reasons.push(IneligibleReason::WorkerRevoked),
            WorkerHealth::Offline => reasons.push(IneligibleReason::WorkerOffline),
            WorkerHealth::Online => {}
        }
        match snapshot {
            None => reasons.push(IneligibleReason::CapabilityUnreported),
            Some(s) if s.expires_at_ms <= now => reasons.push(IneligibleReason::CapabilityExpired),
            Some(_) => {}
        }
        reasons.extend(load.ceiling.map(reason));
        let node_idle = load.total.map(|t| t.saturating_sub(load.occupied));
        if health == WorkerHealth::Online {
            match load.total {
                Some(t) => {
                    total = total.saturating_add(t);
                    idle = idle.saturating_add(node_idle.unwrap_or(0));
                }
                None => unadvertised = true,
            }
        }
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
        out.push(NodeCapacity {
            node: EntityRef {
                kind: "worker".into(),
                id: node.id,
                revision: None,
            },
            name: node.name.clone(),
            health,
            last_seen_at: node
                .last_seen_at
                .map(|at| timestamp(at.timestamp_millis().max(0) as u64))
                .transpose()?,
            backend_kind: load
                .backend
                .and_then(|k| serde_json::to_value(k).ok())
                .and_then(|v| v.as_str().map(str::to_owned)),
            slots: NodeSlots {
                total: load.total,
                occupied: load.occupied,
                idle: node_idle,
            },
            eligibility: Eligibility {
                state: if reasons.is_empty() {
                    EligibilityState::Eligible
                } else {
                    EligibilityState::Ineligible
                },
                reasons,
            },
            capability_observed_at: snapshot
                .and_then(|s| s.observed_at_ms)
                .map(timestamp)
                .transpose()?,
            capability_fresh_until: snapshot.map(|s| timestamp(s.expires_at_ms)).transpose()?,
            storage,
        });
    }

    let providers = providers(state, &nodes, &snapshots, now)?;
    let mut gaps = Vec::new();
    if unadvertised {
        gaps.push("Some online workers advertise no slot total");
    }
    if truncated {
        gaps.push("Worker inventory exceeds the bounded read");
    }
    let unknown_total = unadvertised || truncated;
    let reason = (!gaps.is_empty()).then(|| gaps.join("; "));
    let slots = SlotSummary {
        total: if unknown_total {
            Total::unknown()
        } else {
            exact(total)
        },
        occupied: exact(u32::try_from(occupied_all).unwrap_or(u32::MAX)),
        idle: if unknown_total {
            Total::unknown()
        } else {
            exact(idle)
        },
        observed_at: timestamp(now)?,
        coverage: Coverage {
            state: if reason.is_some() {
                Completeness::Partial
            } else {
                Completeness::Complete
            },
            reason: reason.clone(),
        },
    };
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

/// Per configured account: available while an unrevoked node holds an
/// unexpired, available binding for one of the account's credential profiles.
fn providers(
    state: &AppState,
    nodes: &[crate::store::Node],
    snapshots: &BTreeMap<Uuid, Snapshot>,
    now: u64,
) -> Result<Vec<ProviderAvailability>> {
    let registry = state.config.intelligence.as_deref();
    let accounts = crate::provider_accounts::project(registry, now).accounts;
    let live: Vec<&Snapshot> = nodes
        .iter()
        .filter(|n| n.revoked_at.is_none())
        .filter_map(|n| snapshots.get(&n.id))
        .filter(|s| s.expires_at_ms > now)
        .collect();
    accounts
        .into_iter()
        .map(|account| {
            let profiles: Vec<&ArtifactRef> = registry
                .map(|r| r.credentials.as_slice())
                .unwrap_or_default()
                .iter()
                .filter(|c| {
                    c.provider == account.provider
                        && c.account_reference == account.account_reference
                })
                .map(|c| &c.profile)
                .collect();
            let (mut reported, mut available_nodes) = (Vec::new(), 0u32);
            for snapshot in &live {
                let bindings: Vec<&Binding> = snapshot
                    .credential_bindings
                    .iter()
                    .filter(|b| b.expires_at_ms > now && profiles.contains(&&b.credential_profile))
                    .collect();
                if bindings.iter().any(|b| b.available) {
                    available_nodes += 1;
                }
                reported.extend(bindings);
            }
            let (state, reason) = if !account.enabled {
                (
                    AvailabilityState::Unavailable,
                    Some(AvailabilityReason::AccountDisabled),
                )
            } else if reported.is_empty() {
                (
                    AvailabilityState::Unknown,
                    Some(AvailabilityReason::NoFreshNodeReport),
                )
            } else if available_nodes > 0 {
                (AvailabilityState::Available, None)
            } else {
                (
                    AvailabilityState::Unavailable,
                    Some(AvailabilityReason::CredentialUnavailableOnNodes),
                )
            };
            Ok(ProviderAvailability {
                provider_id: account.provider,
                account_ref: account.account_reference,
                state,
                reason,
                available_nodes,
                observed_at: reported
                    .iter()
                    .map(|b| b.observed_at_ms)
                    .max()
                    .map(timestamp)
                    .transpose()?,
                fresh_until: reported
                    .iter()
                    .map(|b| b.expires_at_ms)
                    .max()
                    .map(timestamp)
                    .transpose()?,
            })
        })
        .collect()
}
