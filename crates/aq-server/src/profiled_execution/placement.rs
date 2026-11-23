//! Placement facts and backend preference, fenced against current node occupancy.
use crate::AppState;
use crate::error::AppError;
use crate::observations::vm_time::SpendObservation;
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
    observed: SpendObservation,
) -> Vec<(Uuid, Option<Money>)> {
    let mut out = Vec::new();
    for (node, adv) in nodes {
        let Some(limit) = adv.spend_ceiling.as_ref() else {
            continue;
        };
        let spent = if crate::observations::vm_time::is_monthly_backend(adv.kind) {
            crate::observations::vm_time::node(conn, *node, limit, observed)
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
use crate::error::db;

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
    aq_machine_control::slots::active(conn, nodes, exclude_attempt)
        .await
        .map_err(crate::machine_api::mapped)
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

/// Configured compute occupancy and placement ceiling for one node.
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
    let spends = spends(
        conn,
        ledger,
        &advertised,
        SpendObservation {
            now_ms,
            exclude_attempt: None,
        },
    )
    .await;
    let pressured = crate::node_storage::pressured(&mut *conn, nodes).await?;
    let mut loads: Vec<Load> = nodes
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
        .collect();
    if aq_machine_control::slots::available(conn)
        .await
        .map_err(crate::machine_api::mapped)?
    {
        for load in &mut loads {
            let slots = aq_machine_control::slots::view(conn, load.node_id)
                .await
                .map_err(crate::machine_api::mapped)?;
            load.total = Some(slots.total);
            load.occupied = slots.occupied;
        }
    }
    Ok(loads)
}

/// Placement facts for selection on `node_id`, or None for a host-native node.
pub async fn facts(
    state: &AppState,
    node_id: Uuid,
    project_id: Uuid,
    environment: &str,
    now_ms: u64,
) -> Result<Option<aq_selection::Placement>, AppError> {
    let template = template(state, &state.store.pool, environment).await?;
    let book = crate::observations::catalog::rate_book(state);
    facts_in(
        &state.store.pool,
        &Ledger {
            pool: &state.store.observation_pool,
            book: &book,
        },
        policy(state, project_id).as_ref(),
        node_id,
        template.as_deref(),
        environment,
        now_ms,
    )
    .await
}
/// The template digest `environment` resolves to through the catalog, read on
/// `db` (else the operator's allowlist); `None` when it names none or several.
#[cfg_attr(not(feature = "project_environments"), allow(unused_variables))]
pub async fn template<'e>(
    state: &AppState,
    db: impl sqlx::PgExecutor<'e>,
    environment: &str,
) -> Result<Option<String>, AppError> {
    #[cfg(feature = "project_environments")]
    let catalog = super::definitions::catalog(state, db).await?;
    #[cfg(feature = "project_environments")]
    let digest = (catalog.template(environment)).map(|a| a.digest.as_str().to_owned());
    #[cfg(not(feature = "project_environments"))]
    let digest = (state.config.templates.as_deref())
        .and_then(|allowlist| aq_environment_templates::resolve([environment], allowlist).ok())
        .and_then(|resolved| resolved.into_iter().next())
        .map(|r| r.template_digest.as_str().to_owned());
    Ok(digest)
}

pub async fn facts_in(
    pool: &PgPool,
    ledger: &Ledger<'_>,
    policy: Option<&PlacementPolicy>,
    node_id: Uuid,
    template: Option<&str>,
    environment: &str,
    now_ms: u64,
) -> Result<Option<aq_selection::Placement>, AppError> {
    let mut conn = pool.acquire().await.map_err(db)?;
    derive(
        &mut conn,
        ledger,
        policy,
        node_id,
        template,
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

/// First store format carrying the placement tables.
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
    // The template digest the environment resolves to, if any.
    template: Option<&str>,
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
    let mut own: Registered = serde_json::from_value(own).map_err(|_| AppError::Internal)?;
    let policy = policy.cloned().unwrap_or(PlacementPolicy {
        preference: Vec::new(),
        required_capabilities: Vec::new(),
    });
    let preference = if policy.preference.is_empty() {
        DEFAULT_PREFERENCE.to_vec()
    } else {
        policy.preference.clone()
    };
    let resolved = template.map(str::to_owned);

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
    let mut peers: Vec<(Uuid, Registered)> = peers
        .into_iter()
        .filter_map(|(node, reg)| Some((node, serde_json::from_value(reg).ok()?)))
        .collect();
    let mut nodes: Vec<Uuid> = peers.iter().map(|(n, _)| *n).collect();
    nodes.push(node_id);
    let counts = aq_machine_control::slots::aware_counts(conn, &nodes)
        .await
        .map_err(crate::machine_api::mapped)?;
    if let Some(count) = counts.get(&node_id) {
        own.backend.max_concurrent_sandboxes = *count;
    }
    for (node, registration) in &mut peers {
        if let Some(count) = counts.get(node) {
            registration.backend.max_concurrent_sandboxes = *count;
        }
    }
    let active = active(&mut *conn, &nodes, derivation.exclude_attempt).await?;
    let mut advertised: Vec<(Uuid, &BackendAdvertisement)> =
        peers.iter().map(|(n, r)| (*n, &r.backend)).collect();
    advertised.push((node_id, &own.backend));
    let spends = spends(
        conn,
        ledger,
        &advertised,
        SpendObservation {
            now_ms,
            exclude_attempt: derivation.exclude_attempt,
        },
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
        environment_templates: resolved
            .iter()
            .map(|digest| {
                Ok(aq_selection::EnvironmentTemplate {
                    environment: environment.to_owned(),
                    template: convert(digest)?,
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

pub fn peer_scan_truncated(scanned: usize) -> bool {
    i64::try_from(scanned).is_ok_and(|n| n >= MAX_PEERS)
}

/// Admission-fence re-check (offer, preparation and grant replay). The node's
/// placement facts are re-derived inside the fence's SERIALIZABLE transaction
/// from `delegated_node_boots`, the configured template allowlist and the
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
    let template = template(state, &mut *tx, &project.environment).await?;
    let current = derive(
        tx,
        &ledger,
        policy(state, decision.inputs.project.lineage.project_id).as_ref(),
        project.node_id,
        template.as_deref(),
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

pub fn recheck(
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
