//! Swarm read: machines, worker nodes and running agents (spec 0007). Owner
//! only; nothing here grants placement.
use super::{
    contract::*,
    runtime::Runtime,
    service::{auth, envelope, fence, session, stamp, tx},
    *,
};
use crate::{store::User, version_info};
use aq_swarm::assemble::{MAX_AGENTS, MeasuredWindow, NodeExtra, SwarmInput, assemble};

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
    let windows: Vec<MeasuredWindow> = state
        .config
        .profiled_execution
        .as_ref()
        .map(|p| {
            p.context_qualifications
                .iter()
                .map(|q| MeasuredWindow {
                    runtime: q.subject.runtime.clone(),
                    qualification: q.subject.runtime_qualification.clone(),
                    observed_at_ms: q.observed_at_ms,
                    expires_at_ms: q.expires_at_ms,
                })
                .collect()
        })
        .unwrap_or_default();
    let mut tx = tx(runtime).await?;
    let extras = NodeExtra::load(&mut tx, &ids, &windows, now).await?;
    let machines = aq_swarm::machine::latest(&mut tx, &ids, now).await?;
    let services = aq_swarm::service::states(&mut tx, &ids).await?;
    let held = aq_swarm::assemble::running(&mut tx, &ids).await?;
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
        running_agents: cap.data.slots.occupied.value.unwrap_or(0),
    };
    let reason = [
        cap.coverage.reason.clone(),
        (held.len() > MAX_AGENTS).then(|| "More than 64 running agents; some are not shown".into()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<String>>()
    .join("; ");
    let data = assemble(SwarmInput {
        now_ms: now,
        server,
        capacity: cap.data,
        extras,
        machines,
        services,
        agents,
    })?;
    fence(state, runtime, user, &a).await?;
    envelope(
        state,
        scope,
        data,
        Some(reason).filter(|r| !r.is_empty()).as_deref(),
    )
}
