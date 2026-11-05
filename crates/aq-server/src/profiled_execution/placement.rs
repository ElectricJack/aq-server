//! C4 placement facts for a B1-registered sandbox node: the activation-time
//! template pin, the authenticated backend advertisement, node ceilings and
//! the backend placement preference (docker-local, then boat, then
//! cloudflare-containers unless the project policy overrides it). A host-native
//! node without a registration gets no placement facts and is unchanged.
//!
//! The preference hold is bounded and explained: a lower-preference node is
//! refused with `placement_preferred_backend_available` only while a
//! higher-preference eligible peer has an unexpired capability snapshot and
//! free capacity, so the hold ends no later than that snapshot's expiry.
use crate::AppState;
use crate::error::AppError;
use aq_admission_sandbox::sandbox::{BackendAdvertisement, BackendCapability, BackendKind, Money};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

pub const DEFAULT_PREFERENCE: [BackendKind; 3] = [
    BackendKind::DockerLocal,
    BackendKind::Boat,
    BackendKind::CloudflareContainers,
];
const MAX_PEERS: i64 = 256;

/// Optional ProjectPolicy override of placement preference and the backend
/// capabilities the project requires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementPolicy {
    #[serde(default)]
    pub preference: Vec<BackendKind>,
    #[serde(default)]
    pub required_capabilities: Vec<BackendCapability>,
}
impl PlacementPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut seen = Vec::new();
        for kind in &self.preference {
            if seen.contains(kind) {
                return Err("duplicate placement preference backend");
            }
            seen.push(*kind);
        }
        if self.preference.len() > 3 || self.required_capabilities.len() > 4 {
            return Err("placement policy bounds");
        }
        Ok(())
    }
}

/// The node's H2-accounted spend, comparable to its advertised `limit`, or
/// None when it is not comparable. A node that reported no calls has spent
/// zero. Otherwise the ledger total is a lower bound: it counts only when the
/// accounting is complete or already reaches the limit, so unknown costs never
/// put a node under its ceiling. Mixed or other currencies are not comparable.
pub fn accounted(summary: &aq_accounting::pricing::Summary, limit: &Money) -> Option<Money> {
    if summary.costs.is_empty() {
        return Some(Money {
            currency: limit.currency.clone(),
            micros: 0,
        });
    }
    let spent = summary.spent_microunits?;
    let currency = summary.currency.as_ref()?.as_str();
    (currency == limit.currency && (summary.complete || spent >= limit.micros)).then(|| Money {
        currency: currency.to_owned(),
        micros: spent,
    })
}

/// Accounted spend for nodes with ceilings: current-month VM time for Boat,
/// lifetime provider calls otherwise. A read failure leaves the node
/// unaccounted, so it refuses.
async fn spends(
    conn: &mut PgConnection,
    ledger: &Ledger<'_>,
    nodes: &[(Uuid, &BackendAdvertisement)],
    now_ms: u64,
    exclude_attempt: Option<Uuid>,
) -> Vec<(Uuid, Option<Money>)> {
    let mut out = Vec::new();
    for (node, adv) in nodes {
        let Some(limit) = adv.spend_ceiling.as_ref() else {
            continue;
        };
        let spent = if crate::observations::vm_time::is_monthly_backend(adv.kind) {
            crate::observations::vm_time::node(conn, *node, limit, now_ms, exclude_attempt)
                .await
                .ok()
                .flatten()
        } else {
            crate::observations::budget::node(ledger.pool, ledger.book, *node)
                .await
                .ok()
                .and_then(|summary| accounted(&summary, limit))
        };
        out.push((*node, spent));
    }
    out
}
fn spent(spends: &[(Uuid, Option<Money>)], node: Uuid) -> Option<&Money> {
    spends
        .iter()
        .find(|(n, _)| *n == node)
        .and_then(|(_, m)| m.as_ref())
}

/// The H2 observation ledger and rate cards that price node spend.
pub struct Ledger<'a> {
    pub pool: &'a PgPool,
    pub book: &'a aq_accounting::pricing::RateCardBook,
}

/// The concurrency ceiling, then the spend ceiling. A configured spend ceiling
/// with no comparable accounted spend refuses rather than assuming zero.
pub fn ceiling(
    advertisement: &BackendAdvertisement,
    active: u32,
    spend: Option<&Money>,
) -> Option<aq_selection::NodeCeiling> {
    use aq_selection::NodeCeiling;
    if active >= advertisement.max_concurrent_sandboxes {
        return Some(NodeCeiling::Concurrency);
    }
    let limit = advertisement.spend_ceiling.as_ref()?;
    match spend {
        Some(spent) if spent.currency == limit.currency => {
            (spent.micros >= limit.micros).then_some(NodeCeiling::Spend)
        }
        _ => Some(NodeCeiling::SpendUnaccounted),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Peer {
    pub kind: BackendKind,
    /// Online, under its ceilings, with the required capabilities and the
    /// resolved template.
    pub eligible: bool,
}
fn rank(preference: &[BackendKind], kind: BackendKind) -> usize {
    preference
        .iter()
        .position(|k| *k == kind)
        .unwrap_or(preference.len())
}
/// The best eligible peer ranked strictly above `own`; unlisted kinds rank last.
pub fn preferred_peer(
    own: BackendKind,
    preference: &[BackendKind],
    peers: &[Peer],
) -> Option<BackendKind> {
    let own_rank = rank(preference, own);
    peers
        .iter()
        .filter(|p| p.eligible && rank(preference, p.kind) < own_rank)
        .min_by_key(|p| rank(preference, p.kind))
        .map(|p| p.kind)
}

fn convert<T: serde::de::DeserializeOwned>(value: impl Serialize) -> Result<T, AppError> {
    serde_json::from_value(serde_json::to_value(value).map_err(|_| AppError::Internal)?)
        .map_err(|_| AppError::Internal)
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}

fn policy(state: &AppState, project_id: Uuid) -> Option<PlacementPolicy> {
    state
        .config
        .profiled_execution
        .as_ref()?
        .projects
        .iter()
        .find(|p| p.project_id == project_id)?
        .placement
        .clone()
}

async fn active(
    conn: &mut PgConnection,
    nodes: &[Uuid],
    exclude_attempt: Option<Uuid>,
) -> Result<Vec<(Uuid, i64)>, AppError> {
    sqlx::query_as(
        "SELECT node_id, count(*) FROM execution_attempts WHERE holds_task AND node_id = ANY($1) \
         AND attempt_id IS DISTINCT FROM $2 GROUP BY node_id",
    )
    .bind(nodes)
    .bind(exclude_attempt)
    .fetch_all(conn)
    .await
    .map_err(db)
}
fn count(active: &[(Uuid, i64)], node: Uuid) -> u32 {
    active
        .iter()
        .find(|(n, _)| *n == node)
        .map_or(0, |(_, c)| u32::try_from(*c).unwrap_or(u32::MAX))
}
/// The stored registration was validated when the node registered; placement
/// reads only its authenticated backend advertisement.
#[derive(Deserialize)]
struct Registered {
    backend: BackendAdvertisement,
}

/// One node's slot load for the capacity read model: the advertised slot total
/// (None for a host-native node), task-holding attempts, and the node ceiling
/// placement would apply now.
#[derive(Debug, Clone)]
pub struct Load {
    pub node_id: Uuid,
    pub backend: Option<BackendKind>,
    pub total: Option<u32>,
    pub occupied: u32,
    pub ceiling: Option<aq_selection::NodeCeiling>,
}

/// Loads for `nodes`, read on `conn` with the same ceiling as [`derive`]. A
/// malformed registration is reported as host-native rather than failing.
pub async fn loads(
    conn: &mut PgConnection,
    ledger: &Ledger<'_>,
    nodes: &[Uuid],
    now_ms: u64,
) -> Result<Vec<Load>, AppError> {
    let rows: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(
        "SELECT node_id, registration FROM delegated_node_boots WHERE node_id = ANY($1)",
    )
    .bind(nodes)
    .fetch_all(&mut *conn)
    .await
    .map_err(db)?;
    let registered: Vec<(Uuid, Registered)> = rows
        .into_iter()
        .filter_map(|(node, reg)| Some((node, serde_json::from_value(reg).ok()?)))
        .collect();
    let active = active(&mut *conn, nodes, None).await?;
    let advertised: Vec<(Uuid, &BackendAdvertisement)> =
        registered.iter().map(|(n, r)| (*n, &r.backend)).collect();
    let spends = spends(conn, ledger, &advertised, now_ms, None).await;
    let pressured = crate::node_storage::pressured(&mut *conn, nodes).await?;
    Ok(nodes
        .iter()
        .map(|&node| {
            let adv = advertised.iter().find(|(n, _)| *n == node).map(|(_, a)| *a);
            let occupied = count(&active, node);
            Load {
                node_id: node,
                backend: adv.map(|a| a.kind),
                total: adv.map(|a| a.max_concurrent_sandboxes),
                occupied,
                ceiling: adv.and_then(|a| {
                    ceiling(a, occupied, spent(&spends, node)).or(pressured
                        .contains(&node)
                        .then_some(aq_selection::NodeCeiling::DiskPressure))
                }),
            }
        })
        .collect())
}

/// Placement facts for selection on `node_id`, or None for a host-native node.
pub async fn facts(
    state: &AppState,
    node_id: Uuid,
    catalog_digest: &str,
    project_id: Uuid,
    environment: &str,
    now_ms: u64,
) -> Result<Option<aq_selection::Placement>, AppError> {
    let book = crate::observations::catalog::rate_book(state);
    facts_in(
        &state.store.pool,
        &Ledger {
            pool: &state.store.observation_pool,
            book: &book,
        },
        policy(state, project_id).as_ref(),
        node_id,
        catalog_digest,
        environment,
        now_ms,
    )
    .await
}

pub async fn facts_in(
    pool: &PgPool,
    ledger: &Ledger<'_>,
    policy: Option<&PlacementPolicy>,
    node_id: Uuid,
    catalog_digest: &str,
    environment: &str,
    now_ms: u64,
) -> Result<Option<aq_selection::Placement>, AppError> {
    let mut conn = pool.acquire().await.map_err(db)?;
    derive(
        &mut conn,
        ledger,
        policy,
        node_id,
        catalog_digest,
        environment,
        now_ms,
        Derivation::SELECTION,
    )
    .await
}

/// What a derivation reads beyond the node's own registration, pins and load.
#[derive(Debug, Clone, Copy)]
pub struct Derivation {
    /// Scan peers for the preference hold. Selection only: once a node holds a
    /// reservation, a newly eligible preferred peer does not take it away.
    pub peers: bool,
    /// The fenced attempt's own reservation, which must not count toward its
    /// node's concurrency ceiling.
    pub exclude_attempt: Option<Uuid>,
}
impl Derivation {
    pub const SELECTION: Self = Self {
        peers: true,
        exclude_attempt: None,
    };
}

/// First store format carrying every table `derive` reads: `delegated_node_boots`
/// (migration 50), `profile_catalog_environment_templates` (55) and
/// `node_storage_reports` (67).
pub const PLACEMENT_FORMAT: u32 = 67;

/// Whether this store carries placement facts. A store held at an earlier
/// migration prefix has no sandbox registrations, so placement is absent (as
/// for an unregistered node) instead of failing the whole poll or fence.
async fn placement_recorded(conn: &mut PgConnection) -> Result<bool, AppError> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut *conn)
            .await
            .map_err(db)?;
    Ok(version.parse::<u32>().map_err(|_| AppError::Internal)? >= PLACEMENT_FORMAT)
}

/// Placement facts read on `conn`, so an admission fence reads them inside its
/// SERIALIZABLE transaction.
#[allow(clippy::too_many_arguments)]
pub async fn derive(
    conn: &mut PgConnection,
    ledger: &Ledger<'_>,
    policy: Option<&PlacementPolicy>,
    node_id: Uuid,
    catalog_digest: &str,
    environment: &str,
    now_ms: u64,
    derivation: Derivation,
) -> Result<Option<aq_selection::Placement>, AppError> {
    if !placement_recorded(&mut *conn).await? {
        return Ok(None);
    }
    let own: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT registration FROM delegated_node_boots WHERE node_id=$1")
            .bind(node_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db)?;
    let Some(own) = own else {
        return Ok(None);
    };
    let own: Registered = serde_json::from_value(own).map_err(|_| AppError::Internal)?;
    let policy = policy.cloned().unwrap_or(PlacementPolicy {
        preference: Vec::new(),
        required_capabilities: Vec::new(),
    });
    let preference = if policy.preference.is_empty() {
        DEFAULT_PREFERENCE.to_vec()
    } else {
        policy.preference.clone()
    };
    let resolutions = crate::templates::load(&mut *conn, catalog_digest).await?;
    let resolved = resolutions
        .iter()
        .find(|r| r.environment == environment)
        .map(|r| r.template_digest.as_str().to_owned());

    let peers: Vec<(Uuid, serde_json::Value)> = if derivation.peers {
        sqlx::query_as(
            "SELECT b.node_id, b.registration FROM delegated_node_boots b \
             JOIN intelligence_node_snapshots s ON s.node_id=b.node_id \
             WHERE b.node_id<>$1 AND (s.snapshot->>'expires_at_ms')::bigint > $2 \
             ORDER BY b.node_id LIMIT $3",
        )
        .bind(node_id)
        .bind(i64::try_from(now_ms).unwrap_or(i64::MAX))
        .bind(MAX_PEERS)
        .fetch_all(&mut *conn)
        .await
        .map_err(db)?
    } else {
        Vec::new()
    };
    if peer_scan_truncated(peers.len()) {
        // Peers past the bound are never considered for the hold, so a
        // preferred peer may be missed and this node offered work instead.
        tracing::warn!(
            target: "aq_server::placement",
            node_id = %node_id,
            peer_scan_bound = MAX_PEERS,
            "placement peer scan reached its bound; later peers were not considered for the preference hold"
        );
    }
    // A malformed peer registration is never a reason to hold.
    let peers: Vec<(Uuid, Registered)> = peers
        .into_iter()
        .filter_map(|(node, reg)| Some((node, serde_json::from_value(reg).ok()?)))
        .collect();
    let mut nodes: Vec<Uuid> = peers.iter().map(|(n, _)| *n).collect();
    nodes.push(node_id);
    let active = active(&mut *conn, &nodes, derivation.exclude_attempt).await?;
    let mut advertised: Vec<(Uuid, &BackendAdvertisement)> =
        peers.iter().map(|(n, r)| (*n, &r.backend)).collect();
    advertised.push((node_id, &own.backend));
    let spends = spends(
        conn,
        ledger,
        &advertised,
        now_ms,
        derivation.exclude_attempt,
    )
    .await;
    // SBX25: a node above the disk refusal line is in the ceiling family.
    let pressured = crate::node_storage::pressured(&mut *conn, &nodes).await?;
    let limit = |adv: &BackendAdvertisement, node: Uuid| {
        ceiling(adv, count(&active, node), spent(&spends, node)).or(pressured
            .contains(&node)
            .then_some(aq_selection::NodeCeiling::DiskPressure))
    };
    let eligible = |adv: &BackendAdvertisement, node: Uuid| {
        limit(adv, node).is_none()
            && policy
                .required_capabilities
                .iter()
                .all(|c| adv.capabilities.contains(c))
            && resolved
                .as_deref()
                .is_some_and(|d| adv.templates.iter().any(|t| t.digest.as_str() == d))
    };
    let peers: Vec<Peer> = peers
        .iter()
        .map(|(node, reg)| Peer {
            kind: reg.backend.kind,
            eligible: eligible(&reg.backend, *node),
        })
        .collect();
    let backend = &own.backend;
    Ok(Some(aq_selection::Placement {
        capabilities: backend
            .capabilities
            .iter()
            .map(convert)
            .collect::<Result<_, _>>()?,
        required_capabilities: policy
            .required_capabilities
            .iter()
            .map(convert)
            .collect::<Result<_, _>>()?,
        templates: backend
            .templates
            .iter()
            .map(|t| {
                Ok(aq_selection::AdvertisedTemplate {
                    digest: convert(&t.digest)?,
                    harnesses: t
                        .harnesses
                        .iter()
                        .map(|h| aq_selection::AdvertisedHarness {
                            harness: h.harness.clone(),
                            version: h.version.clone(),
                        })
                        .collect(),
                })
            })
            .collect::<Result<_, AppError>>()?,
        environment_templates: resolutions
            .iter()
            .map(|r| {
                Ok(aq_selection::EnvironmentTemplate {
                    environment: r.environment.clone(),
                    template: convert(&r.template_digest)?,
                })
            })
            .collect::<Result<_, AppError>>()?,
        ceiling: limit(backend, node_id),
        preferred_peer: preferred_peer(backend.kind, &preference, &peers)
            .map(convert)
            .transpose()?,
        backend: Some(convert(backend.kind)?),
    }))
}

fn peer_scan_truncated(scanned: usize) -> bool {
    i64::try_from(scanned).is_ok_and(|n| n >= MAX_PEERS)
}

/// Admission-fence re-check (offer, preparation and grant replay). The node's
/// placement facts are re-derived inside the fence's SERIALIZABLE transaction
/// from `delegated_node_boots`, `profile_catalog_environment_templates` and the
/// node's active attempts, then the frozen choice is re-checked with the same
/// typed reasons as selection. A ceiling reached or a template lost since
/// selection refuses with 409 and the reason code. The preference hold is
/// selection-only and is not re-derived here.
pub(crate) async fn fence_locked(
    state: &AppState,
    tx: &mut PgConnection,
    decision: &crate::intelligence::resolver::Decision,
    owned_attempt: Option<Uuid>,
    now_ms: u64,
) -> Result<(), AppError> {
    let project = &decision.project;
    let book = crate::observations::catalog::rate_book(state);
    let ledger = Ledger {
        pool: &state.store.observation_pool,
        book: &book,
    };
    let current = derive(
        tx,
        &ledger,
        policy(state, decision.inputs.project.lineage.project_id).as_ref(),
        project.node_id,
        &project.project.binding.catalog_digest,
        &project.environment,
        now_ms,
        Derivation {
            peers: false,
            exclude_attempt: owned_attempt,
        },
    )
    .await?;
    let choice = &decision
        .outcome
        .resolved
        .as_ref()
        .ok_or_else(|| AppError::Conflict("decision has no eligible selection".into()))?
        .choice;
    recheck(decision.inputs.placement.as_ref(), current.as_ref(), choice)
}

fn recheck(
    selected: Option<&aq_selection::Placement>,
    current: Option<&aq_selection::Placement>,
    choice: &aq_selection::ConcreteChoice,
) -> Result<(), AppError> {
    match (selected, current) {
        (None, None) => Ok(()),
        // Selection did not see this node's sandbox registration, or saw one
        // that is gone; either way its placement facts no longer apply.
        (None, Some(_)) | (Some(_), None) => {
            Err(AppError::Conflict("placement_registration_changed".into()))
        }
        (Some(_), Some(current)) => aq_selection::recheck(current, choice)
            .map_err(|reason| AppError::Conflict(reason.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_selection::NodeCeiling;

    fn adv(max: u32, spend_ceiling: Option<(&str, u64)>) -> BackendAdvertisement {
        serde_json::from_value(serde_json::json!({
            "kind": "boat",
            "capabilities": ["vm-isolation"],
            "max_concurrent_sandboxes": max,
            "spend_ceiling": spend_ceiling.map(|(c, m)| serde_json::json!({"currency": c, "micros": m})),
            "templates": [],
        }))
        .unwrap()
    }
    fn money(currency: &str, micros: u64) -> Money {
        Money {
            currency: currency.into(),
            micros,
        }
    }

    #[test]
    fn placement_prefers_local_then_boat_then_cloudflare() {
        use BackendKind::*;
        let peer = |kind, eligible| Peer { kind, eligible };
        let all = [
            peer(DockerLocal, true),
            peer(Boat, true),
            peer(CloudflareContainers, true),
        ];
        // Default order: only the most preferred eligible backend gets offers.
        assert_eq!(preferred_peer(DockerLocal, &DEFAULT_PREFERENCE, &all), None);
        assert_eq!(
            preferred_peer(Boat, &DEFAULT_PREFERENCE, &all),
            Some(DockerLocal)
        );
        assert_eq!(
            preferred_peer(CloudflareContainers, &DEFAULT_PREFERENCE, &all),
            Some(DockerLocal)
        );
        // An ineligible (offline, full or unsupported) local node holds nothing.
        let no_local = [
            peer(DockerLocal, false),
            peer(Boat, true),
            peer(CloudflareContainers, true),
        ];
        assert_eq!(preferred_peer(Boat, &DEFAULT_PREFERENCE, &no_local), None);
        assert_eq!(
            preferred_peer(CloudflareContainers, &DEFAULT_PREFERENCE, &no_local),
            Some(Boat)
        );
        let only_cloud = [peer(DockerLocal, false), peer(Boat, false)];
        assert_eq!(
            preferred_peer(CloudflareContainers, &DEFAULT_PREFERENCE, &only_cloud),
            None
        );
        // ProjectPolicy override reverses the order; unlisted kinds rank last.
        let override_ = [CloudflareContainers, Boat];
        assert_eq!(preferred_peer(CloudflareContainers, &override_, &all), None);
        assert_eq!(
            preferred_peer(DockerLocal, &override_, &all),
            Some(CloudflareContainers)
        );
        assert_eq!(
            preferred_peer(Boat, &override_, &all),
            Some(CloudflareContainers)
        );
        // A peer of the same backend never holds.
        assert_eq!(
            preferred_peer(Boat, &DEFAULT_PREFERENCE, &[peer(Boat, true)]),
            None
        );
        // Policy validation.
        let p = |preference: Vec<BackendKind>| PlacementPolicy {
            preference,
            required_capabilities: vec![],
        };
        assert!(p(vec![Boat, DockerLocal]).validate().is_ok());
        assert!(p(vec![Boat, Boat]).validate().is_err());
        let policy: PlacementPolicy = serde_json::from_value(serde_json::json!({
            "preference": ["cloudflare-containers"],
            "required_capabilities": ["nested-docker"]
        }))
        .unwrap();
        assert_eq!(policy.preference, [CloudflareContainers]);
    }

    #[test]
    fn node_at_spend_ceiling_refuses_with_reason() {
        let usd = |m| money("USD", m);
        assert_eq!(ceiling(&adv(2, None), 1, None), None);
        assert_eq!(
            ceiling(&adv(2, None), 2, None),
            Some(NodeCeiling::Concurrency)
        );
        let limited = adv(2, Some(("USD", 1_000)));
        assert_eq!(ceiling(&limited, 0, Some(&usd(999))), None);
        assert_eq!(
            ceiling(&limited, 0, Some(&usd(1_000))),
            Some(NodeCeiling::Spend)
        );
        assert_eq!(NodeCeiling::Spend.reason(), "node_spend_ceiling_reached");
        // Unaccounted or incomparable spend never degrades to "under ceiling".
        assert_eq!(
            ceiling(&limited, 0, None),
            Some(NodeCeiling::SpendUnaccounted)
        );
        assert_eq!(
            ceiling(&limited, 0, Some(&money("EUR", 0))),
            Some(NodeCeiling::SpendUnaccounted)
        );
        // Concurrency is checked first.
        assert_eq!(
            ceiling(&limited, 2, Some(&usd(5_000))),
            Some(NodeCeiling::Concurrency)
        );
    }

    #[test]
    fn accounted_node_spend_never_assumes_unknown_costs_are_zero() {
        use aq_accounting::pricing::Summary;
        use aq_accounting::wire::{ArtifactRef, Cost, Digest, Text, UnknownReason};
        let text = |s: &str| Text::new(s).unwrap();
        let reported = |micros| Cost::ProviderReported {
            amount_microunits: micros,
            currency: text("USD"),
            receipt: ArtifactRef {
                id: text("receipt"),
                version: text("1"),
                digest: Digest::of(b"receipt"),
            },
        };
        let unknown = Cost::Unknown {
            reason: UnknownReason::NotReported,
        };
        let summary =
            |costs: Vec<Cost>, spent: Option<u64>, currency: Option<&str>, complete| Summary {
                costs,
                spent_microunits: spent,
                currency: currency.map(text),
                complete,
                reason: None,
            };
        let limit = money("USD", 1_000);
        // Money has no PartialEq; compare its parts.
        let acc = |s: &Summary| accounted(s, &limit).map(|m| (m.currency, m.micros));
        let usd = |m| Some(("USD".to_owned(), m));
        // No reported calls: the node has spent nothing, in the limit currency.
        assert_eq!(acc(&summary(vec![], Some(0), None, false)), usd(0));
        // Complete accounting counts as is, under or at the limit.
        let complete = |m| summary(vec![reported(m)], Some(m), Some("USD"), true);
        assert_eq!(acc(&complete(999)), usd(999));
        assert_eq!(acc(&complete(1_000)), usd(1_000));
        // An incomplete lower bound counts only once it reaches the limit.
        let partial = |m| {
            summary(
                vec![reported(m), unknown.clone()],
                Some(m),
                Some("USD"),
                false,
            )
        };
        assert_eq!(acc(&partial(999)), None);
        assert_eq!(acc(&partial(1_000)), usd(1_000));
        // Only unknown costs, mixed currencies or another currency: unaccounted.
        assert_eq!(
            acc(&summary(vec![unknown.clone()], Some(0), None, false)),
            None
        );
        assert_eq!(
            acc(&summary(vec![reported(1), reported(2)], None, None, false)),
            None
        );
        assert_eq!(
            acc(&summary(vec![reported(5)], Some(5), Some("EUR"), true)),
            None
        );
        // Fed into the ceiling: under receives offers, at refuses with a reason.
        let limited = adv(2, Some(("USD", 1_000)));
        let spent = accounted(&complete(999), &limit);
        assert_eq!(ceiling(&limited, 0, spent.as_ref()), None);
        let spent = accounted(&complete(1_000), &limit);
        assert_eq!(
            ceiling(&limited, 0, spent.as_ref()),
            Some(NodeCeiling::Spend)
        );
        let spent = accounted(&partial(999), &limit);
        assert_eq!(
            ceiling(&limited, 0, spent.as_ref()),
            Some(NodeCeiling::SpendUnaccounted)
        );
    }

    #[tokio::test]
    async fn sandbox_facts_pin_template_and_hold_for_preferred_peer() {
        let db = crate::test_support::TestDb::create().await;
        let pool = db.store().await.pool;
        let hex = |c: char| std::iter::repeat_n(c, 64).collect::<String>();
        let user = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users(id,email,role,created_at) VALUES($1,'o@example.test','owner',now())",
        )
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms) VALUES($1,'\x7b\x7d',$2,1)")
            .bind(hex('f')).bind(user).execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        crate::templates::store(
            &mut tx,
            &hex('f'),
            &[crate::templates::Resolution {
                environment: "ci@1".into(),
                template_digest: serde_json::from_value(serde_json::json!(hex('a'))).unwrap(),
            }],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let node = |kind: &str, expires: u64| {
            let pool = pool.clone();
            let kind = kind.to_owned();
            let template = hex('a');
            async move {
                let id = Uuid::new_v4();
                sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'n',$2,'t',1,now())")
                    .bind(id).bind(id.to_string()).execute(&pool).await.unwrap();
                let backend = serde_json::json!({"backend":{"kind":kind,"capabilities":["vm-isolation"],"max_concurrent_sandboxes":1,"spend_ceiling":null,
                    "templates":[{"digest":template,"harnesses":[{"harness":"codex","version":"1"}],"estimated_cold_start_ms":1,"estimated_cold_start_cost":{"currency":"USD","micros":0}}]}});
                sqlx::query("INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)")
                    .bind(id).bind(Uuid::new_v4()).bind(backend).execute(&pool).await.unwrap();
                sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,1,1,$3)")
                    .bind(id).bind(Uuid::new_v4()).bind(serde_json::json!({"expires_at_ms":expires})).execute(&pool).await.unwrap();
                id
            }
        };
        let cloud = node("cloudflare-containers", 2_000).await;
        let catalog = hex('f');
        let book = aq_accounting::pricing::RateCardBook::default();
        let ledger = Ledger {
            pool: &pool,
            book: &book,
        };
        let facts = |id, now| facts_in(&pool, &ledger, None, id, &catalog, "ci@1", now);
        let only = facts(cloud, 1_000).await.unwrap().unwrap();
        assert_eq!(only.preferred_peer, None);
        assert_eq!(only.ceiling, None);
        assert_eq!(only.capabilities, ["vm-isolation"]);
        assert_eq!(only.environment_templates[0].template.as_str(), hex('a'));
        assert_eq!(only.templates[0].harnesses[0].version, "1");
        let boat = node("boat", 1_500).await;
        let held = facts(cloud, 1_000).await.unwrap().unwrap();
        assert_eq!(held.preferred_peer, Some(aq_selection::BackendKind::Boat));
        // The hold is bounded by the peer's snapshot expiry.
        assert_eq!(
            facts(cloud, 1_600).await.unwrap().unwrap().preferred_peer,
            None
        );
        // A required capability the peer lacks makes it ineligible; no hold.
        let nested = PlacementPolicy {
            preference: vec![],
            required_capabilities: vec![BackendCapability::NestedDocker],
        };
        let refused = facts_in(
            &pool,
            &ledger,
            Some(&nested),
            cloud,
            &hex('f'),
            "ci@1",
            1_000,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(refused.preferred_peer, None);
        assert_eq!(refused.required_capabilities, ["nested-docker"]);
        assert!(
            facts(boat, 1_000)
                .await
                .unwrap()
                .unwrap()
                .preferred_peer
                .is_none()
        );
        // SBX25: a node above 85% used refuses with node_disk_pressure, and
        // a pressured peer is no reason to hold work for it.
        let disk = |id: Uuid, used: u64| {
            let pool = pool.clone();
            async move {
                let report = crate::node_storage::StorageReport {
                    disk_total_bytes: 100,
                    disk_used_bytes: used,
                    disk_free_bytes: 100 - used,
                    ..Default::default()
                };
                let mut conn = pool.acquire().await.unwrap();
                crate::node_storage::record(&mut conn, &id.to_string(), Some(id), &report, 1)
                    .await
                    .unwrap();
            }
        };
        disk(boat, 85).await;
        assert_eq!(facts(boat, 1_000).await.unwrap().unwrap().ceiling, None);
        disk(boat, 86).await;
        let pressured = facts(boat, 1_000).await.unwrap().unwrap();
        assert_eq!(pressured.ceiling, Some(NodeCeiling::DiskPressure));
        assert_eq!(NodeCeiling::DiskPressure.reason(), "node_disk_pressure");
        assert_eq!(
            facts(cloud, 1_000).await.unwrap().unwrap().preferred_peer,
            None
        );
        // Host-native nodes carry no placement facts.
        assert_eq!(facts(Uuid::new_v4(), 1_000).await.unwrap(), None);
    }

    fn refusal(result: Result<(), AppError>) -> Option<String> {
        match result {
            Ok(()) => None,
            Err(AppError::Conflict(reason)) => Some(reason),
            Err(other) => panic!("unexpected fence error {other:?}"),
        }
    }

    #[tokio::test]
    async fn fence_rechecks_ceiling_and_template_after_selection() {
        let db = crate::test_support::TestDb::create().await;
        let pool = db.store().await.pool;
        let hex = |c: char| std::iter::repeat_n(c, 64).collect::<String>();
        let user = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users(id,email,role,created_at) VALUES($1,'o@example.test','owner',now())",
        )
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
        let catalog = hex('f');
        sqlx::query("INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms) VALUES($1,'\x7b\x7d',$2,1)")
            .bind(&catalog).bind(user).execute(&pool).await.unwrap();
        let pin = |template: Option<String>| {
            let pool = pool.clone();
            let catalog = catalog.clone();
            async move {
                let resolutions: Vec<_> = template
                    .into_iter()
                    .map(|t| crate::templates::Resolution {
                        environment: "ci@1".into(),
                        template_digest: serde_json::from_value(serde_json::json!(t)).unwrap(),
                    })
                    .collect();
                let mut tx = pool.begin().await.unwrap();
                crate::templates::store(&mut tx, &catalog, &resolutions)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
        };
        pin(Some(hex('a'))).await;
        let registration = |kind: &str, templates: serde_json::Value| {
            serde_json::json!({"backend":{"kind":kind,"capabilities":["vm-isolation"],
                "max_concurrent_sandboxes":1,"spend_ceiling":null,"templates":templates}})
        };
        let template_a = serde_json::json!([{"digest":hex('a'),"harnesses":[{"harness":"codex","version":"1"}],
            "estimated_cold_start_ms":1,"estimated_cold_start_cost":{"currency":"USD","micros":0}}]);
        let node = |kind: &str| {
            let pool = pool.clone();
            let registration = registration(kind, template_a.clone());
            async move {
                let id = Uuid::new_v4();
                sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'n',$2,'t',1,now())")
                    .bind(id).bind(id.to_string()).execute(&pool).await.unwrap();
                sqlx::query("INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)")
                    .bind(id).bind(Uuid::new_v4()).bind(registration).execute(&pool).await.unwrap();
                sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,1,1,$3)")
                    .bind(id).bind(Uuid::new_v4()).bind(serde_json::json!({"expires_at_ms":5_000})).execute(&pool).await.unwrap();
                id
            }
        };
        let boat = node("boat").await;
        let artifact =
            |id: &str, digest: String| serde_json::json!({"id":id,"version":"1","digest":digest});
        let choice: aq_selection::ConcreteChoice = serde_json::from_value(serde_json::json!({
            "candidate_id":"c","profile_candidate_id":"p","provider":"openai","model":"m",
            "native_reasoning":"high","reasoning_configuration_reference":"r",
            "harness":"codex","cli_version":"1",
            "adapter":artifact("adapter",hex('1')),"environment":artifact("ci",hex('a')),
            "qualification":artifact("q",hex('2')),"failure_classifier":null,
            "credential_profile":artifact("cred",hex('3')),
            "account_reference":"acct","endpoint_reference":"ep","composition_digest":hex('4'),
            "resources":{"context_bytes":1,"wall_time_ms":1,"memory_bytes":1,"cpu_millis":1,
                "process_limit":1,"cost_limit_microunits":null,"currency":null,
                "require_hard_cost_enforcement":false},
            "max_output_tokens":1,"estimate":null
        }))
        .unwrap();
        let fence_facts = |exclude_attempt| {
            let pool = pool.clone();
            let catalog = catalog.clone();
            async move {
                let mut conn = pool.acquire().await.unwrap();
                let book = aq_accounting::pricing::RateCardBook::default();
                let ledger = Ledger {
                    pool: &pool,
                    book: &book,
                };
                let derivation = Derivation {
                    peers: false,
                    exclude_attempt,
                };
                derive(
                    &mut conn, &ledger, None, boat, &catalog, "ci@1", 1_000, derivation,
                )
                .await
                .unwrap()
            }
        };
        let book = aq_accounting::pricing::RateCardBook::default();
        let ledger = Ledger {
            pool: &pool,
            book: &book,
        };
        let selected = facts_in(&pool, &ledger, None, boat, &catalog, "ci@1", 1_000)
            .await
            .unwrap();
        assert!(selected.is_some());
        let at_fence = |current: Option<aq_selection::Placement>| {
            refusal(recheck(selected.as_ref(), current.as_ref(), &choice))
        };
        assert_eq!(at_fence(fence_facts(None).await), None);
        // Host-native stays unchecked; a registration appearing or vanishing
        // since selection refuses.
        assert_eq!(refusal(recheck(None, None, &choice)), None);
        let changed = Some("placement_registration_changed".to_owned());
        assert_eq!(at_fence(None), changed);
        assert_eq!(
            refusal(recheck(None, fence_facts(None).await.as_ref(), &choice)),
            changed
        );

        // The preference hold is selection-only: a preferred peer coming
        // online after selection does not refuse the fence.
        node("docker-local").await;
        let held = facts_in(&pool, &ledger, None, boat, &catalog, "ci@1", 1_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            held.preferred_peer,
            Some(aq_selection::BackendKind::DockerLocal)
        );
        assert_eq!(fence_facts(None).await.unwrap().preferred_peer, None);
        assert_eq!(at_fence(fence_facts(None).await), None);

        // The node reached its concurrency ceiling since selection.
        let attempt = Uuid::new_v4();
        sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,state) VALUES($1,$2,$3,$4,$5,'{}')")
            .bind(attempt).bind(Uuid::new_v4()).bind(boat).bind(Uuid::new_v4()).bind(Uuid::new_v4())
            .execute(&pool).await.unwrap();
        assert_eq!(
            at_fence(fence_facts(None).await).as_deref(),
            Some("node_concurrency_ceiling_reached")
        );
        // The fenced attempt's own reservation does not count against it.
        assert_eq!(at_fence(fence_facts(Some(attempt)).await), None);
        sqlx::query("UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .unwrap();

        // A later activation re-pinned or dropped the environment's template.
        pin(Some(hex('b'))).await;
        assert_eq!(
            at_fence(fence_facts(None).await).as_deref(),
            Some("environment_template_digest_mismatch")
        );
        pin(None).await;
        assert_eq!(
            at_fence(fence_facts(None).await).as_deref(),
            Some("environment_template_unresolved")
        );
        pin(Some(hex('a'))).await;
        assert_eq!(at_fence(fence_facts(None).await), None);

        // The node's registration no longer advertises the template.
        sqlx::query("UPDATE delegated_node_boots SET registration=$2 WHERE node_id=$1")
            .bind(boat)
            .bind(registration("boat", serde_json::json!([])))
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            at_fence(fence_facts(None).await).as_deref(),
            Some("template_not_advertised")
        );
        assert!(!peer_scan_truncated(255));
        assert!(peer_scan_truncated(256));
    }
}
