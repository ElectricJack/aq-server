//! Spec 0006 phase 0: the question, comment and wait routes check the grant
//! the attempt holds, record the decision and refuse with the reason. The
//! decision itself is `aq-agent-grants`.
use super::*;
use aq_agent_grants::{
    AttemptIds, AuditEntry, Decision, GrantSet, Holder, Refusal, RefusalCode, Target,
};

tokio::task_local! {
    /// Set while the command surface forwards a call it already checked and
    /// audited: the route's own check runs again but writes no second row.
    pub(crate) static DISPATCHED: ();
}

/// An authenticated attempt, as the command surface sees it.
pub(crate) struct AgentCaller {
    pub(crate) ids: AttemptIds,
    pub(crate) grants: GrantSet,
    pub(crate) binding: Value,
    principal: Principal,
}
pub(crate) async fn agent_caller(
    state: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<AgentCaller, RefusalCode> {
    let mut p = authenticate(state, headers)
        .await
        .map_err(|e| RefusalCode::from_agent_access(e.0))?;
    let ids = ids(&p).map_err(|_| RefusalCode::Internal)?;
    let policy = project_grants(state, ids.project_id);
    if !policy.by_work_kind.is_empty() {
        let workspace = id(&p.binding, "workspace_id").map_err(|_| RefusalCode::Internal)?;
        let rt = runtime(state).map_err(|_| RefusalCode::Internal)?;
        let node = super::human::graph_actions::current(rt, workspace, ids.task_id, "task")
            .await
            .map_err(|_| RefusalCode::Internal)?;
        if let Some(kind) = policy.work_kind(work_kind(&node, ids.project_id)) {
            p.grants = kind.narrow(&p.grants);
        }
    }
    Ok(AgentCaller {
        ids,
        grants: p.grants.clone(),
        binding: p.binding.clone(),
        principal: p,
    })
}
impl AgentCaller {
    /// `supervisor` is the current supervisor conversation, if one exists.
    pub(crate) fn whoami(&self, supervisor: Option<Uuid>) -> Value {
        let p = &self.principal;
        let expires =
            chrono::DateTime::from_timestamp_millis(p.expires as i64).map(|t| t.to_rfc3339());
        json!({"api_version":1,"principal":"agent","attempt_id":self.ids.attempt_id,"task_id":self.ids.task_id,
            "project_id":self.ids.project_id,"session_id":p.binding["session_id"],"session_generation":p.binding["generation"],
            "workspace_id":p.binding["workspace_id"],"supervisor_conversation_id":supervisor,
            "profile_id":p.profile["profile_id"],"grants":self.grants,"grants_digest":self.grants.digest(),
            "credential":{"version":p.version.to_string(),"expires_at":expires,"authority_revision":p.revision.to_string()}})
    }
    /// The grant and scope check for one command (§6.4), with its target.
    pub(crate) async fn authorize(
        &self,
        state: &AppState,
        spec: &aq_command_registry::CommandSpec,
        params: &mut serde_json::Map<String, Value>,
    ) -> (Decision, Target) {
        use aq_agent_grants::Relation;
        let own = self.ids.task_id;
        if aq_command_registry::is_draft_action(spec.name) {
            params
                .entry("project_id")
                .or_insert_with(|| json!(self.ids.project_id));
        }
        let me = aq_task_comments::Author::Agent {
            session_id: id(&self.binding, "session_id").unwrap_or_default(),
            attempt_id: self.ids.attempt_id,
        };
        let pool = &state.store.pool;
        let task = match aq_command_registry::target_comment(spec.target, params) {
            Some(c) => aq_task_comments::authored_task(pool, c, self.ids.project_id, &me)
                .await
                .ok()
                .flatten(),
            None => aq_command_registry::target_task(spec.target, params),
        };
        // A comment the caller did not write is in no scope it holds.
        let rel = match task {
            Some(t) if t == own => Relation::Own,
            Some(t) => relation(state, own, t).await,
            None => Relation::Other,
        };
        let holder = Holder {
            project: self.ids.project_id,
        };
        aq_command_registry::authorize(spec, params, &self.grants, holder, own, rel)
    }
}
/// `task` relative to the attempt's task: a task an agent created under it
/// (at the recorded depth), or any other task.
pub(crate) async fn relation(state: &AppState, own: Uuid, task: Uuid) -> aq_agent_grants::Relation {
    let depth: Option<i32> = sqlx::query_scalar("SELECT depth FROM agent_created_tasks WHERE task_id=$1 AND root_task_id=$2 AND depth IS NOT NULL")
        .bind(task).bind(own).fetch_optional(&state.store.pool).await.ok().flatten();
    depth.map_or(aq_agent_grants::Relation::Other, |d| {
        aq_agent_grants::Relation::Descendant(d as u32)
    })
}
/// Only the work host writes these task metadata fields.
pub(crate) fn work_kind(node: &Value, project: Uuid) -> Option<&str> {
    let metadata = &node["metadata"];
    if metadata["aq:project_id"] != json!(project)
        || metadata["aq:playbook_namespace"]
            .as_str()
            .and_then(|id| id.parse::<Uuid>().ok())
            .is_none()
    {
        return None;
    }
    metadata["aq:work_kind"].as_str()
}
/// The project's agent-grant policy (`ProjectPolicy.agent_grants`).
pub(crate) fn project_grants(
    state: &AppState,
    project: Uuid,
) -> aq_agent_grants::resolve::ProjectGrants {
    let projects = state
        .config
        .profiled_execution
        .as_ref()
        .map(|c| c.projects.as_slice());
    let policy = projects
        .unwrap_or_default()
        .iter()
        .find(|p| p.project_id == project);
    policy
        .and_then(|p| p.agent_grants.clone())
        .unwrap_or_default()
}
/// Live narrowing (§6.3): a lowered project ceiling applies at the next call.
pub(super) fn narrow(state: &AppState, project: Uuid, sealed: GrantSet) -> GrantSet {
    project_grants(state, project)
        .ceiling()
        .map_or(sealed.clone(), |c| sealed.intersect(&c))
}
/// False only for a historical test store older than migration 0098 (no grant tables).
#[allow(clippy::needless_return)]
pub(crate) fn grant_tables(_state: &AppState) -> bool {
    #[cfg(feature = "test-support")]
    return _state.store.fixture_profile.is_none_or(|v| v >= 98);
    #[cfg(not(feature = "test-support"))]
    true
}
/// The grants a native launch seals: profile, project policy, and the
/// creating attempt's grants when an agent created the task.
pub(crate) async fn resolve_native(
    tx: &mut Transaction<'_, Postgres>,
    state: &AppState,
    effective: &[(&str, &str)],
    project: Uuid,
    task: Uuid,
    kind: Option<&str>,
) -> std::result::Result<GrantSet, sqlx::Error> {
    use aq_agent_grants::resolve::{Permission, from_permissions, resolve_work};
    if !grant_tables(state) {
        return Ok(GrantSet::empty());
    }
    let perms: Vec<_> = effective
        .iter()
        .map(|(o, s)| Permission {
            operation: o,
            scope_reference: s,
        })
        .collect();
    let creator: Option<Value> = sqlx::query_scalar("SELECT g.grants FROM agent_created_tasks c JOIN agent_launch_grants g USING(attempt_id) WHERE c.task_id=$1")
        .bind(task).fetch_optional(&mut **tx).await?;
    // A creator whose grants cannot be read holds none, so the child gets none.
    let creator = creator.map(|v| sealed(Some(v)).unwrap_or_default());
    Ok(resolve_work(
        &from_permissions(&perms, project),
        &project_grants(state, project),
        creator.as_ref(),
        kind,
    ))
}

/// Seal `grants` beside a new launch binding, in its transaction. No row holds none.
pub(super) async fn seal(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    g: &GrantSet,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO agent_launch_grants(attempt_id,grants,grants_digest) VALUES($1,$2,$3)",
    )
    .bind(attempt)
    .bind(serde_json::to_value(g)?)
    .bind(g.digest())
    .execute(&mut **tx)
    .await?;
    Ok(())
}
pub(super) fn sealed(column: Option<Value>) -> Result<GrantSet> {
    column.map_or(Ok(GrantSet::empty()), |v| {
        GrantSet::new(serde_json::from_value(v)?).map_err(|_| Error("internal"))
    })
}
pub(super) fn ids(p: &Principal) -> Result<AttemptIds> {
    Ok(AttemptIds {
        attempt_id: id(&p.binding, "attempt_id")?,
        task_id: id(&p.binding, "task_id")?,
        project_id: id(&p.binding, "project_id")?,
    })
}
/// Check the grant `path` needs for `p`; Err is the refusal response. `task`
/// is the task thread a comment route names when it is not the attempt's own.
pub(super) async fn require(
    state: &AppState,
    headers: &HeaderMap,
    p: &Principal,
    path: &str,
    task: Option<Uuid>,
) -> std::result::Result<(), Response> {
    let request_id = Uuid::new_v4();
    let ids = ids(p).map_err(|e| e.respond(request_id))?;
    let holder = Holder {
        project: ids.project_id,
    };
    let rel = match task.filter(|t| *t != ids.task_id) {
        Some(t) => relation(state, ids.task_id, t).await,
        None => aq_agent_grants::Relation::Own,
    };
    let (command, grant, target) = aq_agent_grants::route_grant(path, holder, rel)
        .ok_or_else(|| Error("internal").respond(request_id))?;
    let decision = aq_agent_grants::check(&p.grants, holder, command, grant, target);
    if DISPATCHED.try_with(|_| ()).is_err() {
        audit(
            state,
            &AuditEntry::agent(ids, command, grant, target, &decision, request_id),
        )
        .await;
    }
    match decision {
        Decision::Allow(_) => Ok(()),
        Decision::Deny(refusal) => Err(refused(headers, refusal, request_id)),
    }
}
/// One `command_audit` row. Best effort: a failed audit write is logged and
/// never turns a refusal into an allow or an allow into an error.
pub(crate) async fn audit(state: &AppState, e: &AuditEntry) {
    let r = sqlx::query("INSERT INTO command_audit(audit_id,recorded_at_ms,caller_kind,caller_id,attempt_id,task_id,project_id,command,grant_operation,target,decision,reason_code,operation_id,request_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
        .bind(Uuid::new_v4()).bind(now(state) as i64).bind(e.caller_kind).bind(e.caller_id)
        .bind(e.attempt_id).bind(e.task_id).bind(e.project_id).bind(&e.command).bind(&e.grant)
        .bind(&e.target).bind(if e.allowed { "allow" } else { "deny" }).bind(e.reason_code)
        .bind(e.operation_id).bind(e.request_id)
        .execute(&state.store.pool).await;
    if let Err(error) = r {
        tracing::warn!(%error, command = %e.command, "command audit row not recorded");
    }
}
/// The §6.8 body for a client that sends `X-AQ-Refusal-Version: 1`; otherwise
/// the closed agent-access-1 `ErrorResponse`, whose message names the grant.
pub(crate) fn refused(headers: &HeaderMap, r: Refusal, request_id: Uuid) -> Response {
    let status = StatusCode::from_u16(r.code.status()).unwrap_or(StatusCode::FORBIDDEN);
    if headers
        .get("X-AQ-Refusal-Version")
        .is_some_and(|v| v == "1")
    {
        return response(status, r.with_request(request_id).body());
    }
    response(status, r.legacy_body(request_id))
}
