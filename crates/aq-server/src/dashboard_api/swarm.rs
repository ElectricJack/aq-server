//! Swarm read: machines, worker nodes and slot-holding attempts (spec 0007). Owner
//! only; nothing here grants placement.
use super::{
    contract::*,
    service::{auth, envelope, fence, stamp, tx},
    *,
};
use crate::agent_access::human::{self as graph, ReadRuntime};
use crate::{store::User, version_info};
use aq_swarm::assemble::{MAX_CANDIDATES, NodeExtra, SwarmInput, assemble};

fn projects_of(state: &AppState) -> Vec<Uuid> {
    (state.config.profiled_execution.as_ref())
        .map(|c| c.enabled_projects())
        .unwrap_or_default()
}
pub(super) async fn swarm(request: &DashboardRequest, scope: Scope) -> Result<Envelope<Swarm>> {
    let (state, runtime, user) = request.parts();
    let a = auth(state, runtime, user).await?;
    if !a.owner {
        return Err(AppError::Forbidden("swarm requires owner".into()).into());
    }
    let cap = super::capacity::capacity(request, scope.clone()).await?;
    let ids: Vec<Uuid> = cap.data.nodes.iter().map(|n| n.node.id).collect();
    let now = stamp(state)?;
    let mut tx = tx(runtime).await?;
    // Placement needs no measurement (spec 0014 §8), so none is ever a hold.
    let mut extras = NodeExtra::load(&mut tx, &ids, &[], now).await?;
    extras.iter_mut().for_each(|e| e.measurement = None);
    let machines = aq_swarm::machine::latest(&mut tx, &ids, now).await?;
    let services = aq_swarm::service::states(&mut tx, &ids).await?;
    let controls = aq_swarm::service::controls(&mut tx, &ids).await?;
    let owner_availability = aq_swarm::service::owner_availability(&mut tx).await?;
    let online: Vec<Uuid> = (cap.data.nodes.iter())
        .filter(|n| n.health == WorkerHealth::Online)
        .map(|n| n.node.id)
        .collect();
    let projects = projects_of(state);
    let snapshot = history_snapshot(state, now);
    let history = aq_history_caps::swarm_holds(&mut tx, &online, &projects, snapshot).await?;
    // R6: tasks a server-side hold keeps from being offered, per project and reason.
    let task_holds = aq_machine_control::holds::counts(&mut tx, &projects)
        .await
        .map_err(|_| Error::Unavailable)?;
    let query = aq_swarm::agents::Query {
        nodes: &ids,
        project: scope.project(),
        runtime: state.config.terminals.as_ref().map(|t| t.runtime_id),
        summaries: crate::role_settings::tables(state),
        now_ms: now,
        limit: MAX_CANDIDATES,
    };
    let mut held = aq_swarm::agents::load(&mut tx, query).await?;
    tx.commit().await?;
    task_titles(state, user, &mut held).await;
    let info = version_info();
    let server = SwarmServer {
        version: info.server_version,
        source_revision: info.source_revision,
        running_agents: u32::try_from(held.running).unwrap_or(u32::MAX),
        owner_availability,
    };
    let reason = aq_swarm::assemble::reason(cap.coverage.reason.clone(), held.cut);
    let data = assemble(SwarmInput {
        now_ms: now,
        server,
        capacity: cap.data,
        extras,
        machines,
        services,
        controls,
        agents: held.agents,
        history,
        task_holds,
    })?;
    let mut data = aq_swarm::management::apply(&state.store.pool, data).await?;
    aq_swarm::routing::apply(&mut *state.store.pool.acquire().await?, &mut data).await?;
    for machine in &mut data.machines {
        for node in &mut machine.nodes {
            node.inventory_rejection = state
                .store
                .inventory_diagnostics
                .get(node.node.id)
                .rejection;
        }
    }
    fence(state, runtime, user, &a).await?;
    envelope(state, scope, data, reason.as_deref())
}
async fn task_titles(state: &AppState, user: &User, held: &mut aq_swarm::agents::Held) {
    let reader =
        (state.agent_access.is_some()).then(|| ReadRuntime::with_pool(state.store.pool.clone()));
    let until = tokio::time::Instant::now() + aq_swarm::agents::TITLE_BUDGET;
    held.titles(async |project, task| {
        let read = graph::task_brief(state, reader.as_ref()?, user, project, task);
        let brief = tokio::time::timeout_at(until, read).await.ok()?;
        Some(brief.ok().and_then(|b| b.title))
    })
    .await;
}
fn history_snapshot(state: &AppState, now: u64) -> aq_history_caps::Snapshot {
    let staging = state
        .source_retention
        .as_ref()
        .map(|r| (r.config.max_staging_bytes, r.config.max_bundle_bytes));
    aq_history_caps::Snapshot::new(now, staging)
}
