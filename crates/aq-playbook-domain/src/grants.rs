use crate::{Error, GrantScope, Namespace, Result, digest};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRequest {
    pub grant_id: Uuid,
    pub scope: GrantScope,
    pub expires_at_ms: i64,
    pub resume_delivery_operation_id: Option<Uuid>,
}

/// Retain a namespace owner's grant under the caller's namespace lock.
/// An optional approval and its grant commit together, using the original decision identity.
pub async fn retain(
    db: &mut PgConnection,
    ns: &Namespace,
    id: Uuid,
    scope: &GrantScope,
    expires: i64,
    now: i64,
    resume: Option<Uuid>,
) -> Result<()> {
    let actor = ns.owner_user_id;
    let fields = (ns.namespace_id, ns.generation, id, scope, expires, actor);
    let payload = match resume {
        Some(operation) => digest(&(fields, operation))?,
        None => digest(&fields)?,
    };
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT payload_digest FROM playbook_grants WHERE grant_id=$1",
    )
    .bind(id)
    .fetch_optional(&mut *db)
    .await?;
    if let Some(old) = existing {
        return if old == payload {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    if expires <= now || expires > now.saturating_add(86_400_000) {
        return Err(Error::Invalid);
    }
    sqlx::query("INSERT INTO playbook_grants(grant_id,namespace_id,generation,revision,owner_user_id,scope,expires_at_ms,payload_digest) VALUES($1,$2,$3,1,$4,$5,$6,$7)")
        .bind(id).bind(ns.namespace_id).bind(ns.generation).bind(actor)
        .bind(serde_json::json!(scope)).bind(expires).bind(payload).execute(&mut *db).await?;
    if let Some(operation) = resume {
        let inserted = sqlx::query(
            "INSERT INTO playbook_delivery_resumptions(operation_id,grant_id,grant_revision,approved_at_ms)
             SELECT e.operation_id,g.grant_id,g.revision,$3
             FROM playbook_effects e JOIN playbook_grants old ON old.grant_id=e.grant_id
             JOIN playbook_grants g ON g.grant_id=$2 AND g.namespace_id=e.namespace_id
                AND g.generation=e.generation AND g.owner_user_id=old.owner_user_id
                AND g.scope=old.scope
             WHERE e.operation_id=$1 AND e.status='pending' AND old.revision=e.grant_revision
               AND old.namespace_id=e.namespace_id AND old.generation=e.generation
               AND (old.revoked_at_ms IS NOT NULL OR old.expires_at_ms<=$3)
               AND e.command_name IN ('work.complete','work.return')
               AND NOT playbook_run_finished(e.namespace_id,e.generation,e.run_id)",
        )
        .bind(operation)
        .bind(id)
        .bind(now)
        .execute(db)
        .await?
        .rows_affected();
        if inserted != 1 {
            return Err(Error::Forbidden);
        }
    }
    Ok(())
}

pub async fn delivery_resume_current(
    db: &mut PgConnection,
    operation: Uuid,
    at: i64,
) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT playbook_delivery_resume_current($1,$2)")
            .bind(operation)
            .bind(at)
            .fetch_one(db)
            .await?,
    )
}

pub async fn scope_locked(
    db: &mut PgConnection,
    ns: &Namespace,
    id: Uuid,
    revision: i64,
    live_at: impl FnOnce() -> Result<Option<i64>>,
) -> Result<GrantScope> {
    let row = sqlx::query("SELECT * FROM playbook_grants WHERE grant_id=$1 AND namespace_id=$2 AND generation=$3 FOR UPDATE")
        .bind(id).bind(ns.namespace_id).bind(ns.generation).fetch_optional(db).await?
        .ok_or(Error::Forbidden)?;
    if row.get::<i64, _>("revision") != revision
        || row.get::<Uuid, _>("owner_user_id") != ns.owner_user_id
        || live_at()?.is_some_and(|at| {
            row.get::<Option<i64>, _>("revoked_at_ms").is_some()
                || row.get::<i64, _>("expires_at_ms") <= at
        })
    {
        return Err(Error::Forbidden);
    }
    Ok(serde_json::from_value(row.get("scope"))?)
}
