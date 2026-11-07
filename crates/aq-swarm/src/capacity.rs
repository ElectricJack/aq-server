//! Capacity assembly: per-node slots and eligibility, the slot summary and
//! provider-account availability. Pure over already-read facts.
use crate::{TimeError, exact, timestamp};
use aq_dashboard_contract::*;
use aq_selection::{ArtifactRef, NodeCeiling};
use serde::Deserialize;
use uuid::Uuid;

/// The capability-snapshot facts capacity needs. Unknown snapshot fields are
/// ignored: the owner validated the full snapshot when the node reported it.
#[derive(Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub observed_at_ms: Option<u64>,
    pub expires_at_ms: u64,
    #[serde(default)]
    pub credential_bindings: Vec<Binding>,
}
#[derive(Deserialize)]
pub struct Binding {
    pub credential_profile: ArtifactRef,
    pub available: bool,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}

/// `worker_status` text to health; anything but online/revoked is offline.
pub fn health(status: &str) -> WorkerHealth {
    match status {
        "online" => WorkerHealth::Online,
        "revoked" => WorkerHealth::Revoked,
        _ => WorkerHealth::Offline,
    }
}

pub fn millis(at: chrono::DateTime<chrono::Utc>) -> u64 {
    at.timestamp_millis().max(0) as u64
}

/// Unexpired snapshots of the given (unrevoked) items.
pub fn live<'a, T>(
    items: impl Iterator<Item = T>,
    snapshot: impl Fn(T) -> Option<&'a Snapshot>,
    now_ms: u64,
) -> Vec<&'a Snapshot> {
    items
        .filter_map(snapshot)
        .filter(|s| s.expires_at_ms > now_ms)
        .collect()
}

pub fn ineligible_reason(ceiling: NodeCeiling) -> IneligibleReason {
    match ceiling {
        NodeCeiling::Concurrency => IneligibleReason::NodeConcurrencyCeilingReached,
        NodeCeiling::Spend => IneligibleReason::NodeSpendCeilingReached,
        NodeCeiling::SpendUnaccounted => IneligibleReason::NodeSpendUnaccounted,
        NodeCeiling::DiskPressure => IneligibleReason::NodeDiskPressure,
    }
}

pub struct NodeInput<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub health: WorkerHealth,
    pub last_seen_ms: Option<u64>,
    pub backend_kind: Option<String>,
    pub total: Option<u32>,
    pub occupied: u32,
    pub ceiling: Option<NodeCeiling>,
    pub snapshot: Option<&'a Snapshot>,
    pub storage: Option<NodeStorage>,
}

pub fn node_capacity(input: NodeInput<'_>, now_ms: u64) -> Result<NodeCapacity, TimeError> {
    let NodeInput {
        id,
        name,
        health,
        last_seen_ms,
        backend_kind,
        total,
        occupied,
        ceiling,
        snapshot,
        storage,
    } = input;
    let mut reasons = Vec::new();
    match health {
        WorkerHealth::Revoked => reasons.push(IneligibleReason::WorkerRevoked),
        WorkerHealth::Offline => reasons.push(IneligibleReason::WorkerOffline),
        WorkerHealth::Online => {}
    }
    match snapshot {
        None => reasons.push(IneligibleReason::CapabilityUnreported),
        Some(s) if s.expires_at_ms <= now_ms => reasons.push(IneligibleReason::CapabilityExpired),
        Some(_) => {}
    }
    reasons.extend(ceiling.map(ineligible_reason));
    Ok(NodeCapacity {
        node: EntityRef {
            kind: "worker".into(),
            id,
            revision: None,
        },
        name: name.to_owned(),
        health,
        last_seen_at: last_seen_ms.map(timestamp).transpose()?,
        backend_kind,
        slots: NodeSlots {
            total,
            occupied,
            idle: total.map(|t| t.saturating_sub(occupied)),
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
    })
}

pub fn slot_summary(
    nodes: &[NodeCapacity],
    occupied_all: u32,
    truncated: bool,
    now_ms: u64,
) -> Result<(SlotSummary, Option<String>), TimeError> {
    let (mut total, mut idle, mut unadvertised) = (0u32, 0u32, false);
    for node in nodes.iter().filter(|n| n.health == WorkerHealth::Online) {
        match node.slots.total {
            Some(t) => {
                total = total.saturating_add(t);
                idle = idle.saturating_add(node.slots.idle.unwrap_or(0));
            }
            None => unadvertised = true,
        }
    }
    let mut gaps = Vec::new();
    if unadvertised {
        gaps.push("Some online workers advertise no slot total");
    }
    if truncated {
        gaps.push("Worker inventory exceeds the bounded read");
    }
    let unknown_total = unadvertised || truncated;
    let reason = (!gaps.is_empty()).then(|| gaps.join("; "));
    let summary = SlotSummary {
        total: if unknown_total {
            Total::unknown()
        } else {
            exact(total)
        },
        occupied: exact(occupied_all),
        idle: if unknown_total {
            Total::unknown()
        } else {
            exact(idle)
        },
        observed_at: timestamp(now_ms)?,
        coverage: Coverage {
            state: if reason.is_some() {
                Completeness::Partial
            } else {
                Completeness::Complete
            },
            reason: reason.clone(),
        },
    };
    Ok((summary, reason))
}

pub struct AccountInput {
    pub provider: String,
    pub account_reference: String,
    pub enabled: bool,
    pub profiles: Vec<ArtifactRef>,
}

/// Per configured account: available while an unrevoked node holds an
/// unexpired, available binding for one of the account's credential profiles.
pub fn provider_availability(
    accounts: Vec<AccountInput>,
    live: &[&Snapshot],
    now_ms: u64,
) -> Result<Vec<ProviderAvailability>, TimeError> {
    accounts
        .into_iter()
        .map(|account| {
            let (mut reported, mut available_nodes) = (Vec::new(), 0u32);
            for snapshot in live {
                let bindings: Vec<&Binding> = snapshot
                    .credential_bindings
                    .iter()
                    .filter(|b| {
                        b.expires_at_ms > now_ms && account.profiles.contains(&b.credential_profile)
                    })
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
