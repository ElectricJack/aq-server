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
    grants: &aq_agent_grants::GrantSet,
) -> std::result::Result<(), crate::error::AppError> {
    seal(state, tx, session, profile, context, grants)
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
    grants: &aq_agent_grants::GrantSet,
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
    let p = launch_principal(&a, a.identity.node_id, profile, context)?;
    let binding = p.binding.clone();
    p.issuance_response(&"a".repeat(47))?;
    let inserted=sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING").bind(a.attempt.attempt_id).bind(a.identity.node_id).bind(&binding).bind(profile).bind(context).execute(&mut **tx).await?.rows_affected();
    if inserted == 1 && super::grant_check::grant_tables(state) {
        super::grant_check::seal(tx, a.attempt.attempt_id, grants).await?;
    }
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
