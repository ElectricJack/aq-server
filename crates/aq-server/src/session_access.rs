//! Shared exact-session inspection ACL (watch/peek/logs/usage). It confers NO
//! terminal control or graph authority. Call after common node/attempt locks.
use crate::{AppState, error::AppError, execution::authority::AuthorityView, store::Role};
use aq_session_protocol::{SessionIdentity, canonical_digest};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
#[derive(Clone, Debug)]
pub struct InspectionAccess {
    pub principal_id: Uuid,
    pub session: SessionIdentity,
    pub checked_at_ms: u64,
    pub valid_until_ms: u64,
    pub owner: bool,
}
pub fn session_key(session: &SessionIdentity) -> Result<String, AppError> {
    canonical_digest(session)
        .map(|v| v.as_str().to_owned())
        .map_err(|_| AppError::BadRequest("invalid session identity".into()))
}
pub async fn current_role_locked(
    tx: &mut Transaction<'_, Postgres>,
    actor: Uuid,
) -> Result<Role, AppError> {
    if actor.is_nil() {
        return Err(AppError::Forbidden("session access denied".into()));
    }
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
        .bind(actor)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)?;
    role.ok_or_else(|| AppError::Forbidden("session access denied".into()))?
        .parse()
        .map_err(|_| AppError::Internal)
}
pub async fn require_inspect_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    authority: &AuthorityView,
    principal: Uuid,
) -> Result<InspectionAccess, AppError> {
    let role = current_role_locked(tx, principal).await?;
    let key = session_key(&authority.session)?;
    let row=sqlx::query("SELECT session_identity::text,can_inspect,expires_at_ms FROM session_inspection_acl WHERE session_key=$1 AND principal_id=$2 FOR SHARE").bind(&key).bind(principal).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?;
    let now = (state.clock)().timestamp_millis().max(0) as u64;
    if role == Role::Owner {
        return Ok(InspectionAccess {
            principal_id: principal,
            session: authority.session.clone(),
            checked_at_ms: now,
            valid_until_ms: u64::MAX,
            owner: true,
        });
    }
    let row = row.ok_or_else(|| AppError::Forbidden("session inspection denied".into()))?;
    let exact: SessionIdentity =
        serde_json::from_str(row.get::<&str, _>(0)).map_err(|_| AppError::Internal)?;
    if exact != authority.session {
        return Err(AppError::Internal);
    }
    let expires: i64 = row.get(2);
    if !row.get::<bool, _>(1) || expires <= 0 || now >= expires as u64 {
        return Err(AppError::Forbidden("session inspection denied".into()));
    }
    Ok(InspectionAccess {
        principal_id: principal,
        session: exact,
        checked_at_ms: now,
        valid_until_ms: expires as u64,
        owner: false,
    })
}
