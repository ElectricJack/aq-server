//! Installed node, credential and availability evidence read before selection.
use super::{Result, *};
pub(super) struct NodeEvidence {
    pub node: CapabilitySnapshot,
    pub credentials: Vec<AuthorizedCredential>,
    pub observations: Vec<AvailabilityObservation>,
    pub registry_revision: u64,
    pub time: u64,
}
impl NodeEvidence {
    pub(super) async fn load(state: &AppState, node_id: Uuid) -> Result<Self> {
        let loaded = loaded(state)?;
        let node = node_snapshot(state, node_id).await?;
        let (credentials, time) = eligible_credentials(state, loaded, node_id).await?;
        let observations: Vec<Value> = sqlx::query_scalar(
        "SELECT observation FROM intelligence_observations ORDER BY scope_source_digest LIMIT 1024",
    )
    .fetch_all(&state.store.pool)
    .await
    .map_err(db)?;
        let mut observations = observed(observations, &credentials, node_id)?;
        let registry_revision = sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM intelligence_registry WHERE singleton=TRUE",
        )
        .fetch_optional(&state.store.pool)
        .await
        .map_err(db)?
        .ok_or_else(|| conflict("intelligence registry has not been installed"))?
            as u64;
        // Spec 0007 Q2: the switch's attestation (aq_machine_control::owner_attested).
        let mut conn = state.store.pool.acquire().await.map_err(db)?;
        let at = aq_machine_control::AttestationTime {
            revision: registry_revision,
            now_ms: time,
        };
        let owner = aq_machine_control::owner_attested(&mut conn, &credentials, &observations, at);
        observations.extend(owner.await?);
        drop(conn);
        Ok(Self {
            node,
            credentials,
            observations,
            registry_revision,
            time,
        })
    }
}
async fn node_snapshot(state: &AppState, node_id: Uuid) -> Result<CapabilitySnapshot> {
    let node: Value =
        sqlx::query_scalar("SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1")
            .bind(node_id)
            .fetch_optional(&state.store.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| conflict("installed harness/credential evidence unavailable"))?;
    serde_json::from_value(node).map_err(|_| AppError::Internal)
}
pub(super) fn observed(
    observations: Vec<Value>,
    credentials: &[AuthorizedCredential],
    node_id: Uuid,
) -> Result<Vec<AvailabilityObservation>> {
    let observations = observations
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<Vec<AvailabilityObservation>, _>>()
        .map_err(|_| AppError::Internal)?;
    Ok(relevant(observations, credentials, node_id))
}
async fn eligible_credentials(
    state: &AppState,
    loaded: &config::LoadedRegistry,
    node_id: Uuid,
) -> Result<(Vec<AuthorizedCredential>, u64)> {
    let mut credentials = aq_intelligence_config::node_credentials(loaded, node_id);
    let mut membership = state.store.pool.acquire().await.map_err(db)?;
    aq_swarm_settings::providers::mask(&mut membership, &mut credentials, None)
        .await
        .map_err(db)?;
    let time = now(state)?;
    // A stored Codex login past the owner's age limit is a missing credential here.
    aq_login_store::mask_stale(
        &mut membership,
        loaded,
        &mut credentials,
        aq_login_store::LoginSelection::At(time as i64),
    )
    .await?;
    Ok((credentials, time))
}
