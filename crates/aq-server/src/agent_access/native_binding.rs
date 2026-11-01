//! Internal native offer owner seam. No HTTP provision or credential is minted.
use super::*;
use crate::execution::{
    authority::{Kind, Policy},
    profiled::Status,
};
/// The producer has locked the current authenticated node and sealed the exact
/// immutable launch in this same transaction. Offered authority is sufficient
/// only to retain its graph binding; token issuance still requires GraphActive.
pub(crate) async fn seal_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    session: &aq_session_protocol::SessionIdentity,
    profile: &Value,
    context: &Value,
) -> std::result::Result<(), crate::error::AppError> {
    seal(state, tx, session, profile, context)
        .await
        .map_err(|_| {
            crate::error::AppError::Conflict(
                "native graph binding does not match sealed offer".into(),
            )
        })
}
async fn seal(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    session: &aq_session_protocol::SessionIdentity,
    profile: &Value,
    context: &Value,
) -> Result<()> {
    runtime(state)?;
    if !contract::valid("ResolvedProfile", profile) || !contract::valid("ContextSnapshot", context)
    {
        return Err(Error("invalid_request"));
    }
    let a = crate::execution::authority::load_locked(state, tx, session, Policy::IdentityOnly)
        .await
        .map_err(|_| Error("scope_denied"))?;
    if a.kind != Kind::ProfiledV1
        || !a.holds_task
        || a.status != Status::Offered
        || a.grant_id.is_some()
        || a.revoked
        || a.cancellation_id.is_some()
    {
        return Err(Error("scope_denied"));
    }
    let graph = serde_json::to_value(&a.graph)?;
    if graph.is_null()
        || graph["profile_id"] != profile["profile_id"]
        || graph["profile_revision"] != profile["revision"]
        || graph["profile_digest"] != digest(profile)?
        || graph["context_digest"] != digest(context)?
    {
        return Err(Error("scope_denied"));
    }
    let binding = json!({"task_id":a.task_id,"attempt_id":a.attempt.attempt_id,"session_id":a.attempt.session_id,"generation":a.attempt.generation,"node_id":a.identity.node_id,"boot_id":a.identity.boot_id,"project_id":a.project_id,"workspace_id":graph["workspace_id"],"resolved_spec_digest":a.execution_spec_digest,"profile_digest":graph["profile_digest"],"context_digest":graph["context_digest"]});
    if !contract::valid("Binding", &binding) {
        return Err(Error("invalid_request"));
    }
    let p = Principal {
        created: vec![],
        binding: binding.clone(),
        profile: profile.clone(),
        context: context.clone(),
        version: i64::MAX,
        revision: i64::MAX,
        expires: 253402300799999,
    };
    graph::validate_launch(&p)?;
    p.issuance_response(&"a".repeat(47))?;
    let inserted=sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING").bind(a.attempt.attempt_id).bind(a.identity.node_id).bind(&binding).bind(profile).bind(context).execute(&mut **tx).await?.rows_affected();
    if inserted == 0 {
        let r=sqlx::query("SELECT binding,profile,context,revoked FROM agent_launch_bindings WHERE attempt_id=$1 FOR SHARE").bind(a.attempt.attempt_id).fetch_one(&mut **tx).await?;
        if r.get::<Value, _>("binding") != binding
            || r.get::<Value, _>("profile") != *profile
            || r.get::<Value, _>("context") != *context
            || r.get::<bool, _>("revoked")
        {
            return Err(Error("id_conflict"));
        }
    }
    Ok(())
}
