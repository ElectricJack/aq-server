#[path = "lifecycle.rs"]
mod lifecycle;
#[path = "operations.rs"]
mod operations;
use super::{
    runtime::{self, Resource, Snapshot},
    *,
};
use crate::{
    execution::authority::{self, Kind, Policy},
    session_access,
    store::User,
};
use aq_session_protocol::{ProfiledOffer, SessionIdentity as OwnerSession, Validate};
use chrono::{DateTime, SecondsFormat, Utc};
pub(super) use operations::operations;
#[path = "bootstrap.rs"]
mod startup;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use sqlx::{Postgres, Row, Transaction};
pub(super) use startup::bootstrap;
#[path = "session.rs"]
mod inspected_session;
pub(super) use inspected_session::session;
pub(super) fn stamp(state: &AppState) -> Result<u64> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or(Error::Unavailable)
}
pub(super) fn timestamp(ms: u64) -> Result<Timestamp> {
    let value =
        DateTime::<Utc>::from_timestamp_millis(ms.try_into().map_err(|_| Error::Unavailable)?)
            .ok_or(Error::Unavailable)?;
    Ok(Timestamp(
        value.to_rfc3339_opts(SecondsFormat::Millis, true),
    ))
}
pub(super) async fn tx(runtime: &Runtime) -> Result<Transaction<'_, Postgres>> {
    let mut tx = authority::snapshot(&runtime.pool).await?;
    sqlx::query("SELECT set_config('statement_timeout','500ms',true),set_config('lock_timeout','250ms',true)").execute(&mut *tx).await?;
    Ok(tx)
}
pub(super) struct Auth {
    actor: Uuid,
    pub(super) owner: bool,
    email: String,
    pub(super) partition: String,
    visibility_complete: bool,
}
pub(super) async fn auth(state: &AppState, runtime: &Runtime, user: &User) -> Result<Auth> {
    let mut tx = tx(runtime).await?;
    let row = sqlx::query(
        "SELECT role,email,issuer,subject,xmin::text AS version FROM users WHERE id=$1",
    )
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::AuthorizationChanged)?;
    if row.get::<Option<String>, _>("issuer") != user.issuer
        || row.get::<Option<String>, _>("subject") != user.subject
    {
        return Err(Error::AuthorizationChanged);
    }
    let owner = row.get::<String, _>("role") == "owner";
    let now = stamp(state)?;
    let grants = if owner {
        vec![]
    } else {
        sqlx::query("SELECT session_key,session_identity,revision,expires_at_ms FROM session_inspection_acl WHERE principal_id=$1 AND can_inspect AND expires_at_ms>$2 ORDER BY session_key LIMIT 257").bind(user.id).bind(now as i64).fetch_all(&mut *tx).await?
    };
    let complete = grants.len() <= runtime::MEMBERS;
    let mut pins = vec![];
    for grant in grants.iter().take(runtime::MEMBERS) {
        let identity: OwnerSession = serde_json::from_value(grant.get("session_identity"))
            .map_err(|_| Error::Unavailable)?;
        if session_access::session_key(&identity)? != grant.get::<String, _>("session_key") {
            return Err(Error::Unavailable);
        }
        pins.push(json!([
            grant.get::<String, _>("session_key"),
            grant.get::<i64, _>("revision"),
            grant.get::<i64, _>("expires_at_ms")
        ]));
    }
    let bytes=serde_json::to_vec(&json!({"actor":user.id,"role":row.get::<String,_>("role"),"row_version":row.get::<String,_>("version"),"acl":pins,"bounded":complete})).map_err(|_|Error::Unavailable)?;
    let hash = Sha256::digest(bytes);
    let partition = u128::from_be_bytes(hash[..16].try_into().map_err(|_| Error::Unavailable)?)
        .max(1)
        .to_string();
    let value = Auth {
        actor: user.id,
        owner,
        email: row.get("email"),
        partition,
        visibility_complete: complete,
    };
    tx.commit().await?;
    // Expiry can pass while SQL waits. A pre-query timestamp never fences
    // delivery; refuse rather than retaining a now-expired visibility set.
    let after = stamp(state)?;
    if grants
        .iter()
        .any(|g| g.get::<i64, _>("expires_at_ms") <= after as i64)
    {
        return Err(Error::AuthorizationChanged);
    }
    Ok(value)
}
pub(super) async fn fence(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    expected: &Auth,
) -> Result<()> {
    let current = auth(state, runtime, user).await?;
    if current.partition != expected.partition {
        return Err(Error::AuthorizationChanged);
    }
    Ok(())
}
pub(super) fn envelope<T>(
    state: &AppState,
    scope: Scope,
    data: T,
    reason: Option<&str>,
) -> Result<Envelope<T>> {
    let at = stamp(state)?;
    let partial = reason.is_some();
    Ok(Envelope {
        api_version: ApiVersion::Read1,
        request_id: Uuid::new_v4(),
        server_time: timestamp(at)?,
        scope,
        data,
        observation: Observation {
            observed_at: timestamp(at)?,
            fresh_until: timestamp(at.saturating_add(5000))?,
            source: "aq_dashboard_read".into(),
            quality: Quality::Reported,
            coverage: if partial {
                Completeness::Partial
            } else {
                Completeness::Complete
            },
        },
        pending_operations: vec![],
        coverage: Coverage {
            state: if partial {
                Completeness::Partial
            } else {
                Completeness::Complete
            },
            reason: reason.map(str::to_owned),
        },
    })
}
pub(super) async fn membership(
    request: &DashboardRequest,
    a: &Auth,
    scope: &Scope,
    resource: Resource,
) -> Result<(Vec<Uuid>, bool)> {
    let (state, runtime, _) = request.parts();
    if !a.visibility_complete {
        return Err(Error::AuthorizationChanged);
    }
    let mut tx = tx(runtime).await?;
    let project = scope.project();
    let now = stamp(state)?;
    let mut ids:Vec<Uuid>=match resource {
  Resource::Search => return Err(Error::Invalid),
  Resource::Projects=>{
   if !a.owner {return Err(AppError::Forbidden("project inspection requires owner".into()).into());}
   sqlx::query_scalar("SELECT project_id FROM project_execution_bindings WHERE ($1::uuid IS NULL OR project_id=$1) ORDER BY project_id LIMIT 257").bind(project).fetch_all(&mut *tx).await?
  },
  Resource::Sessions=>sqlx::query_scalar("SELECT e.session_id FROM execution_attempts e WHERE ($1::uuid IS NULL OR CASE WHEN e.kind='profiled_v1' THEN e.state->'offer'->'manifest'->'bindings'->>'project_id' ELSE e.state->'offer'->'spec'->>'project_id' END=$1::text) AND ($2 OR EXISTS(SELECT 1 FROM session_inspection_acl a WHERE a.principal_id=$3 AND a.can_inspect AND a.expires_at_ms>$4 AND a.session_identity = CASE WHEN e.kind='profiled_v1' THEN e.state->'offer'->'binding'->'session' ELSE jsonb_build_object('kind','remote','node_id',e.node_id,'boot_id',e.state->'offer'->'identity'->'boot_id','attempt_id',e.attempt_id,'session_id',e.session_id,'assignment_generation',e.state->'offer'->'attempt'->'generation') END)) ORDER BY e.session_id LIMIT 257").bind(project).bind(a.owner).bind(a.actor).bind(now as i64).fetch_all(&mut *tx).await?
 };
    let truncated = ids.len() > runtime::MEMBERS;
    ids.truncate(runtime::MEMBERS);
    tx.commit().await?;
    Ok((ids, truncated))
}
async fn snapshot(
    request: &DashboardRequest,
    a: &Auth,
    query: &ReadQuery,
    resource: Resource,
) -> Result<(Snapshot, usize)> {
    let (state, runtime, _) = request.parts();
    let scope = query.scope()?;
    let limit = query.limit()?;
    let now = stamp(state)?;
    if let Some(cursor) = &query.cursor {
        return runtime.resume(
            cursor,
            runtime::PageKey {
                actor: a.actor,
                partition: &a.partition,
                scope: &scope,
                resource,
                limit,
                filters: &json!({}),
            },
            now,
        );
    }
    let (ids, truncated) = membership(request, a, &scope, resource).await?;
    let value = Snapshot {
        id: Uuid::nil(),
        actor: a.actor,
        partition: a.partition.clone(),
        scope,
        resource,
        ids,
        filters: json!({}),
        search: None,
        truncated,
        created: now,
        expires: now.checked_add(runtime::TTL).ok_or(Error::Unavailable)?,
    };
    Ok((runtime.insert(value, now)?, 0))
}
fn identity(value: &OwnerSession) -> (Uuid, SessionIdentity) {
    match value {
        OwnerSession::Remote {
            node_id,
            boot_id,
            attempt_id,
            assignment_generation,
            session_id,
        } => (
            *session_id,
            SessionIdentity::Remote {
                node_id: *node_id,
                boot_id: *boot_id,
                attempt_id: *attempt_id,
                session_id: *session_id,
                assignment_generation: Revision(assignment_generation.to_string()),
            },
        ),
        OwnerSession::Central {
            authority_id,
            session_id,
            generation,
        } => (
            *session_id,
            SessionIdentity::Central {
                authority_id: *authority_id,
                session_id: *session_id,
                generation: Revision(generation.to_string()),
            },
        ),
    }
}
fn decimal(value: &str) -> Option<Revision> {
    value
        .parse::<u128>()
        .ok()
        .filter(|n| *n > 0 && n.to_string() == value)
        .map(|_| Revision(value.into()))
}
async fn project_label(
    tx: &mut Transaction<'_, Postgres>,
    project: Uuid,
    owner: bool,
) -> Result<ProjectRef> {
    let label = if owner {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT binding->>'name' FROM project_execution_bindings WHERE project_id=$1",
        )
        .bind(project)
        .fetch_optional(&mut **tx)
        .await?
        .flatten()
        .filter(|s| !s.is_empty() && s.len() <= 512)
        .unwrap_or_else(|| project.to_string())
    } else {
        project.to_string()
    };
    Ok(ProjectRef { id: project, label })
}
pub(super) async fn detail(
    request: &DashboardRequest,
    id: Uuid,
    scope: Scope,
) -> Result<Envelope<SessionDetail>> {
    let (state, runtime, user) = request.parts();
    let a = auth(state, runtime, user).await?;
    if !a.visibility_complete {
        return Err(Error::AuthorizationChanged);
    }
    let data = session(request, id, &scope, None).await?;
    fence(state, runtime, user, &a).await?;
    envelope(
        state,
        scope,
        data,
        Some("Process activity, task titles, and wait details are not connected yet"),
    )
}
fn session_total(snapshot: &Snapshot, skipped: bool) -> Total {
    match (snapshot.truncated, skipped) {
        (true, _) => Total::unknown(),
        (false, skipped) => Total {
            value: Some(snapshot.ids.len() as u32),
            accuracy: if skipped {
                Accuracy::Estimated
            } else {
                Accuracy::Exact
            },
        },
    }
}

pub(super) async fn sessions(
    request: &DashboardRequest,
    query: ReadQuery,
) -> Result<Envelope<Page<SessionSummary>>> {
    let (state, runtime, user) = request.parts();
    let a = auth(state, runtime, user).await?;
    let (snapshot, offset) = snapshot(request, &a, &query, Resource::Sessions).await?;
    let limit = query.limit()?;
    let end = (offset + usize::from(limit)).min(snapshot.ids.len());
    let mut items = vec![];
    let mut skipped = false;
    // Facts of ids this user may not inspect are fetched and never shown:
    // `session` authorizes each row before it uses them.
    let observed =
        crate::observations::freshness(&state.store.pool, &snapshot.ids[offset..end]).await?;
    for id in &snapshot.ids[offset..end] {
        match session(request, *id, &snapshot.scope, Some(&observed)).await {
            Ok(value) => items.push(value.summary),
            Err(Error::Api(AppError::Forbidden(_) | AppError::NotFound(_))) => skipped = true,
            Err(error) => return Err(error),
        }
    }
    fence(state, runtime, user, &a).await?;
    let next = if end < snapshot.ids.len() {
        Some(runtime.next(&snapshot, end, limit)?)
    } else {
        None
    };
    let data = Page {
        items,
        next_cursor: next.clone(),
        has_more: next.is_some(),
        snapshot_id: snapshot.id,
        // The authorized snapshot membership, so the Sessions selector can show
        // a count. A truncated snapshot is unknown; a member that vanished or
        // lost inspection while paging makes the count an estimate.
        total: session_total(&snapshot, skipped),
    };
    envelope(
        state,
        snapshot.scope,
        data,
        Some(if snapshot.truncated {
            "This list is limited; select a project to narrow it"
        } else {
            "Process activity, task titles, and wait details are not connected yet"
        }),
    )
}
pub(super) async fn projects(
    request: &DashboardRequest,
    query: ReadQuery,
) -> Result<Envelope<Page<ProjectRef>>> {
    let (state, runtime, user) = request.parts();
    let a = auth(state, runtime, user).await?;
    if !a.owner {
        return Err(AppError::Forbidden("owner required".into()).into());
    }
    let (snapshot, offset) = snapshot(request, &a, &query, Resource::Projects).await?;
    let limit = query.limit()?;
    let end = (offset + usize::from(limit)).min(snapshot.ids.len());
    let mut items = vec![];
    for id in &snapshot.ids[offset..end] {
        let mut tx = tx(runtime).await?;
        items.push(project_label(&mut tx, *id, true).await?);
        tx.commit().await?;
    }
    fence(state, runtime, user, &a).await?;
    let next = if end < snapshot.ids.len() {
        Some(runtime.next(&snapshot, end, limit)?)
    } else {
        None
    };
    let data = Page {
        items,
        next_cursor: next.clone(),
        has_more: next.is_some(),
        snapshot_id: snapshot.id,
        total: if snapshot.truncated {
            Total::unknown()
        } else {
            Total {
                value: Some(snapshot.ids.len() as u32),
                accuracy: Accuracy::Exact,
            }
        },
    };
    envelope(
        state,
        snapshot.scope,
        data,
        if snapshot.truncated {
            Some("This list is limited; select a project to narrow it")
        } else {
            None
        },
    )
}
pub(super) async fn provider_usage(
    request: &DashboardRequest,
    scope: Scope,
) -> Result<Envelope<Vec<ProviderUsage>>> {
    let (state, runtime, user) = request.parts();
    let a = auth(state, runtime, user).await?;
    if !a.owner {
        return Err(AppError::Forbidden("provider account usage requires owner".into()).into());
    }
    let now = stamp(state)?;
    let inventory = crate::provider_accounts::project(state.config.intelligence.as_deref(), now);
    let (data, partial) = runtime
        .quota
        .collect(inventory.accounts, &state.config.provider_quota, now)
        .await;
    fence(state, runtime, user, &a).await?;
    envelope(
        state,
        scope,
        data,
        partial.then_some("Some account quota observations are unavailable or stale"),
    )
}
