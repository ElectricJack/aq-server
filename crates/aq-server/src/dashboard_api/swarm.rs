//! Swarm read: machines, worker nodes and running agents (spec 0007). Owner
//! only; nothing here grants placement.
use super::{
    contract::*,
    runtime::Runtime,
    service::{auth, envelope, fence, session, stamp, tx},
    *,
};
use crate::execution::authority::{self, Policy};
use crate::{store::User, version_info};
use aq_swarm::assemble::{
    MAX_AGENTS, MAX_CANDIDATES, MeasuredWindow, NodeExtra, SwarmInput, assemble,
};

pub(super) async fn swarm(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    scope: Scope,
) -> Result<Envelope<Swarm>> {
    let a = auth(state, runtime, user).await?;
    if !a.owner {
        return Err(AppError::Forbidden("swarm requires owner".into()).into());
    }
    let cap = super::capacity::capacity(state, runtime, user, scope.clone()).await?;
    let ids: Vec<Uuid> = cap.data.nodes.iter().map(|n| n.node.id).collect();
    let now = stamp(state)?;
    // Renewed facts count: a renewed node does not show measurement_expired.
    let windows: Vec<MeasuredWindow> =
        crate::profiled_execution::context_renewal::current_facts(state, now)
            .await?
            .into_iter()
            .map(|q| MeasuredWindow {
                runtime: q.subject.runtime,
                qualification: q.subject.runtime_qualification,
                observed_at_ms: q.observed_at_ms,
                expires_at_ms: q.expires_at_ms,
            })
            .collect();
    let mut tx = tx(runtime).await?;
    let extras = NodeExtra::load(&mut tx, &ids, &windows, now).await?;
    let machines = aq_swarm::machine::latest(&mut tx, &ids, now).await?;
    let services = aq_swarm::service::states(&mut tx, &ids).await?;
    let controls = aq_swarm::service::controls(&mut tx, &ids).await?;
    let owner_availability = aq_swarm::service::owner_availability(&mut tx).await?;
    // Spec 0008: history caps and the offer rate hold online nodes, visibly.
    let online: Vec<Uuid> = (cap.data.nodes.iter())
        .filter(|n| n.health == WorkerHealth::Online)
        .map(|n| n.node.id)
        .collect();
    let staging = (state.source_retention.as_ref())
        .map(|r| (r.config.max_staging_bytes, r.config.max_bundle_bytes));
    let history = aq_history_caps::swarm_holds(&mut tx, &online, now, staging).await?;
    let mut held = Vec::new();
    // No unlocked authority read exists, so the locking read is bounded: the most recent candidates only.
    let (candidates, cut) = aq_swarm::assemble::holding(&mut tx, &ids, MAX_CANDIDATES).await?;
    for (attempt, id, node) in candidates {
        let v = authority::load_attempt_locked(state, &mut tx, node, attempt, Policy::IdentityOnly)
            .await
            .map_err(|_| Error::Unavailable)?;
        if v.running_at(now) {
            held.push((id, node));
        }
    }
    tx.commit().await?;
    let mut agents = Vec::new();
    for (id, node) in held.iter().take(MAX_AGENTS) {
        match session(state, runtime, user, *id, &scope).await {
            Ok(d) => agents.push((*node, d.summary)),
            Err(Error::Api(AppError::Forbidden(_) | AppError::NotFound(_))) => {}
            Err(e) => return Err(e),
        }
    }
    let info = version_info();
    let server = SwarmServer {
        version: info.server_version,
        source_revision: info.source_revision,
        running_agents: u32::try_from(held.len()).unwrap_or(u32::MAX),
        owner_availability,
    };
    let reason = aq_swarm::assemble::reason(cap.coverage.reason.clone(), held.len(), cut);
    let data = assemble(SwarmInput {
        now_ms: now,
        server,
        capacity: cap.data,
        extras,
        machines,
        services,
        controls,
        agents,
        history,
    })?;
    fence(state, runtime, user, &a).await?;
    envelope(state, scope, data, reason.as_deref())
}
