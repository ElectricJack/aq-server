#[path = "lifecycle.rs"]
mod lifecycle;
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
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use sqlx::{Postgres, Row, Transaction};
use std::collections::BTreeMap;
fn stamp(state: &AppState) -> Result<u64> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or(Error::Unavailable)
}
fn timestamp(ms: u64) -> Result<Timestamp> {
    let value =
        DateTime::<Utc>::from_timestamp_millis(ms.try_into().map_err(|_| Error::Unavailable)?)
            .ok_or(Error::Unavailable)?;
    Ok(Timestamp(
        value.to_rfc3339_opts(SecondsFormat::Millis, true),
    ))
}
async fn tx(runtime: &Runtime) -> Result<Transaction<'_, Postgres>> {
    let mut tx = runtime.pool.begin().await?;
    sqlx::query("SELECT set_config('statement_timeout','500ms',true),set_config('lock_timeout','250ms',true)").execute(&mut *tx).await?;
    Ok(tx)
}
struct Auth {
    actor: Uuid,
    owner: bool,
    email: String,
    partition: String,
    visibility_complete: bool,
}
async fn auth(state: &AppState, runtime: &Runtime, user: &User) -> Result<Auth> {
    let mut tx = tx(runtime).await?;
    let row = sqlx::query(
        "SELECT role,email,issuer,subject,xmin::text AS version FROM users WHERE id=$1 FOR SHARE",
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
        sqlx::query("SELECT session_key,session_identity,revision,expires_at_ms FROM session_inspection_acl WHERE principal_id=$1 AND can_inspect AND expires_at_ms>$2 ORDER BY session_key LIMIT 257 FOR SHARE").bind(user.id).bind(now as i64).fetch_all(&mut *tx).await?
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
async fn fence(state: &AppState, runtime: &Runtime, user: &User, expected: &Auth) -> Result<()> {
    let current = auth(state, runtime, user).await?;
    if current.partition != expected.partition {
        return Err(Error::AuthorizationChanged);
    }
    Ok(())
}
fn capability(name: &str, enabled: bool, reason: &str) -> Capability {
    Capability {
        name: name.into(),
        version: CapabilityVersion::V1,
        enabled,
        reason: if enabled { None } else { Some(reason.into()) },
    }
}
fn envelope<T>(
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
pub(super) async fn bootstrap(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    digest: Option<String>,
    mounted: super::MountedOwners,
) -> Result<Envelope<Bootstrap>> {
    let a = auth(state, runtime, user).await?;
    let graph = mounted.graph && a.owner && state.agent_access.is_some();
    let reviews = a.owner && mounted.reviews;
    let playbooks = a.owner && mounted.playbook_owner == Some(a.actor);
    let caps = vec![
        capability("dashboard.overview", true, ""),
        capability(
            "sessions.read",
            a.visibility_complete,
            "Session inspection is unavailable for this account",
        ),
        capability(
            "session.usage.read",
            mounted.session_usage && a.visibility_complete,
            "Session usage inspection requires a mounted observation owner and bounded session visibility",
        ),
        capability(
            "session.events.read",
            mounted.session_events && a.visibility_complete,
            "Session event inspection requires a mounted observation owner and bounded session visibility",
        ),
        capability(
            "projects.read",
            a.owner,
            "Project inspection requires an owner account",
        ),
        capability("status.read", true, ""),
        capability("workers.read", true, ""),
        capability(
            "notifications.read",
            mounted.notifications,
            "Notification inspection requires a mounted notification owner",
        ),
        capability(
            "human.graph.read",
            graph,
            "Human Graph inspection requires an owner account and configured Graph runtime",
        ),
        capability(
            "human.overview.tasks.read",
            graph,
            "Recorded task summaries require an owner account and configured Graph runtime",
        ),
        capability(
            "tasks.read",
            graph,
            "Task inspection requires an owner account and configured Graph runtime",
        ),
        capability(
            "tasks.create",
            graph && crate::agent_access::human::task_create_available(state),
            "Task creation requires an owner account and configured Graph mutation identity",
        ),
        capability(
            "knowledge.read",
            graph,
            "Knowledge inspection requires an owner account and configured Graph runtime",
        ),
        capability(
            "search.read",
            graph,
            "Search requires an owner account and configured Graph runtime",
        ),
        capability(
            "decisions.read",
            reviews,
            "Decision inspection requires an owner account and a mounted review owner",
        ),
        capability(
            "decisions.resolve",
            reviews,
            "Decision resolution requires an owner account and a mounted review owner",
        ),
        capability(
            "provider.quota",
            false,
            "Provider account usage is unavailable",
        ),
        capability(
            "supervisor.history.read",
            mounted.supervisor_conversation && a.owner,
            "Global supervisor history requires an owner account and mounted conversation owner",
        ),
        capability(
            "supervisor.message.enqueue",
            mounted.supervisor_conversation && a.owner,
            "Global supervisor enqueue requires an owner account and mounted conversation owner",
        ),
        capability(
            "supervisor.conversation",
            false,
            "The global supervisor is not connected yet",
        ),
        capability(
            "terminal.watch",
            state.config.terminals.is_some() && a.visibility_complete,
            "Terminal relay or session inspection is unavailable",
        ),
        capability(
            "terminal.control",
            false,
            "Terminal control is not connected to this dashboard yet",
        ),
        capability(
            "dashboard.actions",
            false,
            "Dashboard actions are not connected yet",
        ),
        capability(
            "results.read",
            mounted.results && a.owner,
            "Result inspection requires an owner account",
        ),
        capability(
            "results.media.read",
            mounted.media_read && a.owner,
            "Supplemental images require an owner account and mounted media storage",
        ),
        capability(
            "results.media.upload",
            mounted.media_upload && a.owner,
            "Supplemental image uploads require an owner account and enabled uploads",
        ),
        capability(
            "playbooks.inspect",
            playbooks,
            "Playbook inspection requires the mounted runtime namespace owner",
        ),
        capability(
            "playbooks.loaded-definitions.read",
            playbooks && mounted.playbook_loaded_definitions,
            "Loaded definitions require the configured runtime namespace owner",
        ),
        capability(
            "playbooks.catalog.read",
            playbooks
                && mounted
                    .playbook_catalog
                    .as_ref()
                    .is_some_and(|support| support.available()),
            "Current catalog inspection requires the mounted component query and namespace owner",
        ),
    ];
    let bytes = contract::schema_bytes();
    let data = Bootstrap {
        principal: Principal {
            id: a.actor,
            display_name: a.email.clone(),
            authorization_revision: Revision(a.partition.clone()),
        },
        product: Product {
            version: env!("CARGO_PKG_VERSION").into(),
            dashboard_digest: digest.map(Digest),
        },
        contracts: vec![ContractRef {
            name: "dashboard-read".into(),
            version: ApiVersion::Read1,
            digest: Digest(format!("sha256:{}", hex::encode(Sha256::digest(bytes)))),
        }],
        capabilities: caps,
        limits: Limits {
            page_size: runtime::PAGE,
            max_page_size: runtime::MAX_PAGE,
            snapshot_members: runtime::MEMBERS as u16,
            snapshot_ttl_ms: runtime::TTL as u32,
        },
        default_scope: Scope::AllAuthorized,
        global_supervisor: Supervisor {
            status: "unconfigured".into(),
            conversation_id: None,
            reason: "The global supervisor is not connected yet".into(),
        },
    };
    fence(state, runtime, user, &a).await?;
    envelope(state, Scope::AllAuthorized, data, None)
}
async fn membership(
    state: &AppState,
    runtime: &Runtime,
    a: &Auth,
    scope: &Scope,
    resource: Resource,
) -> Result<(Vec<Uuid>, bool)> {
    if !a.visibility_complete {
        return Err(Error::AuthorizationChanged);
    }
    let mut tx = tx(runtime).await?;
    let project = scope.project();
    let now = stamp(state)?;
    let mut ids:Vec<Uuid>=match resource {
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
    state: &AppState,
    runtime: &Runtime,
    a: &Auth,
    query: &ReadQuery,
    resource: Resource,
) -> Result<(Snapshot, usize)> {
    let scope = query.scope()?;
    let limit = query.limit()?;
    let now = stamp(state)?;
    if let Some(cursor) = &query.cursor {
        return runtime.resume(
            cursor,
            a.actor,
            &a.partition,
            (&scope, resource),
            limit,
            now,
        );
    }
    let (ids, truncated) = membership(state, runtime, a, &scope, resource).await?;
    let value = Snapshot {
        id: Uuid::nil(),
        actor: a.actor,
        partition: a.partition.clone(),
        scope,
        resource,
        ids,
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
async fn session(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    id: Uuid,
    scope: &Scope,
) -> Result<SessionDetail> {
    if id.is_nil() {
        return Err(Error::Invalid);
    }
    // No transaction spans more than one session or cursor/network work.
    let mut tx = tx(runtime).await?;
    let row = sqlx::query("SELECT attempt_id,node_id FROM execution_attempts WHERE session_id=$1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::NotFound("session not found".into()))?;
    let attempt: Uuid = row.get("attempt_id");
    let node: Uuid = row.get("node_id");
    let view =
        authority::load_attempt_locked(state, &mut tx, node, attempt, Policy::TerminalHistorical)
            .await
            .map_err(AppError::from)?;
    if scope.project().is_some_and(|p| p != view.project_id) {
        return Err(AppError::NotFound("session not found".into()).into());
    }
    let access = session_access::require_inspect_locked(state, &mut tx, &view, user.id).await?;
    let (actual, session_identity) = identity(&view.session);
    if actual != id {
        return Err(Error::Unavailable);
    }
    let project = project_label(&mut tx, view.project_id, access.owner).await?;
    let mut profile = Profile {
        provider: None,
        model: None,
        harness: None,
    };
    let mut revisions = vec![];
    let mut task_revision = None;
    let mut lifecycle = None;
    let mut observation_binding = None;
    if view.kind == Kind::ProfiledV1 {
        // Reuse the existing authorized per-session offer read: receipt PK joins
        // add bounded columns, never another per-row query or an event scan.
        let row=sqlx::query("SELECT e.state->'offer' AS offer,s.request_bytes AS started_request,s.request_digest AS started_digest,s.receipt_bytes AS started_receipt,z.request_bytes AS stopped_request,z.request_digest AS stopped_digest,z.receipt_bytes AS stopped_receipt FROM execution_attempts e LEFT JOIN profiled_started_receipts s USING(attempt_id) LEFT JOIN profiled_stopped_receipts z USING(attempt_id) WHERE e.attempt_id=$1")
            .bind(attempt).fetch_one(&mut *tx).await?;
        let raw: Value = row.get("offer");
        {
            let offer: ProfiledOffer =
                serde_json::from_value(raw).map_err(|_| Error::Unavailable)?;
            offer.validate().map_err(|_| Error::Unavailable)?;
            if offer.binding.session == view.session
                && offer.binding.attempt_id == attempt
                && offer.binding.offer_id == view.offer_id
                && Some(&offer.binding.launch_manifest_digest)
                    == view.launch_manifest_digest.as_ref()
                && offer.binding.execution_spec_digest.as_str() == view.execution_spec_digest
            {
                let binding = aq_admission::transport::lifecycle_binding(&offer)
                    .map_err(|_| Error::Unavailable)?;
                if binding.project_id != view.project_id
                    || binding.task_id != view.task_id
                    || binding.attempt_id != attempt
                    || binding.attempt_generation != view.attempt.generation
                    || binding.session != view.session
                {
                    return Err(Error::Unavailable);
                }
                observation_binding = Some(binding);
                lifecycle = Some(lifecycle::from_row(&row, &offer.binding)?);
                profile = Profile {
                    provider: Some(offer.manifest.model.provider.as_str().into()),
                    model: Some(offer.manifest.model.model.as_str().into()),
                    harness: Some(offer.manifest.harness.harness.as_str().into()),
                };
                task_revision = Some(Revision(offer.manifest.bindings.task_revision.to_string()));
                for (name, reference) in [
                    ("agent", offer.manifest.agent_profile),
                    ("intelligence", offer.manifest.intelligence_profile),
                    ("execution", offer.manifest.execution_profile),
                ] {
                    if let Some(revision) = decimal(reference.version.as_str()) {
                        revisions.push(ProfileRevision {
                            name: name.into(),
                            revision,
                        });
                    }
                }
            } else {
                return Err(Error::Unavailable);
            }
        }
    }
    let mut terminal = None;
    let mut last_activity = None;
    if !view.revoked
        && let Some(config) = state.config.terminals.as_ref()
    {
        let registered=sqlx::query("SELECT terminal_id,identity,terminal_epoch,last_output_at_ms,runtime_id FROM terminal_sessions WHERE attempt_id=$1").bind(attempt).fetch_optional(&mut *tx).await?;
        if let Some(row) = registered {
            let exact: aq_session_protocol::TerminalIdentity =
                serde_json::from_value(row.get("identity")).map_err(|_| Error::Unavailable)?;
            if exact.session == view.session
                && row.get::<Uuid, _>("runtime_id") == config.runtime_id
            {
                terminal = Some(EntityRef {
                    kind: "terminal".into(),
                    id: row.get("terminal_id"),
                    revision: Some(Revision(row.get::<i64, _>("terminal_epoch").to_string())),
                });
                let at: i64 = row.get("last_output_at_ms");
                if at > 0 {
                    last_activity = Some(timestamp(at as u64)?);
                }
            }
        }
    }
    let now = stamp(state)?;
    if now < access.checked_at_ms || now >= access.valid_until_ms {
        return Err(Error::AuthorizationChanged);
    }
    let status = serde_json::to_value(view.status)
        .map_err(|_| Error::Unavailable)?
        .as_str()
        .ok_or(Error::Unavailable)?
        .to_owned();
    let summary = SessionSummary {
        id,
        label: format!(
            "{} session {}",
            if view.kind == Kind::LegacyFixture {
                "Fixture"
            } else {
                "Remote"
            },
            id
        ),
        task: Some(TitledRef {
            kind: "task".into(),
            id: view.task_id,
            revision: task_revision,
            title: view.task_id.to_string(),
        }),
        attempt_id: Some(attempt),
        project,
        profile,
        authority: "remote".into(),
        observation: ProcessObservation {
            state: "unknown".into(),
            observed_at: timestamp(now)?,
            fresh_until: timestamp(now)?,
        },
        wait: None,
        controller: Controller {
            state: "unknown".into(),
            label: None,
        },
        terminal,
        session_identity,
        lifecycle,
        record_revision: if view.record_revision > 0 {
            Some(Revision(view.record_revision.to_string()))
        } else {
            None
        },
        execution_authority: status,
        source_kind: if view.kind == Kind::LegacyFixture {
            "legacy_fixture"
        } else {
            "profiled_v1"
        }
        .into(),
    };
    tx.commit().await?;
    Ok(SessionDetail {
        summary,
        profile_revisions: revisions,
        permissions: vec!["session.inspect".into()],
        last_activity_at: last_activity,
        observation_binding,
    })
}
pub(super) async fn detail(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    id: Uuid,
    scope: Scope,
) -> Result<Envelope<SessionDetail>> {
    let a = auth(state, runtime, user).await?;
    if !a.visibility_complete {
        return Err(Error::AuthorizationChanged);
    }
    let data = session(state, runtime, user, id, &scope).await?;
    fence(state, runtime, user, &a).await?;
    envelope(
        state,
        scope,
        data,
        Some("Process activity, task titles, and wait details are not connected yet"),
    )
}
pub(super) async fn sessions(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    query: ReadQuery,
) -> Result<Envelope<Page<SessionSummary>>> {
    let a = auth(state, runtime, user).await?;
    let (snapshot, offset) = snapshot(state, runtime, &a, &query, Resource::Sessions).await?;
    let limit = query.limit()?;
    let end = (offset + usize::from(limit)).min(snapshot.ids.len());
    let mut items = vec![];

    for id in &snapshot.ids[offset..end] {
        match session(state, runtime, user, *id, &snapshot.scope).await {
            Ok(value) => items.push(value.summary),
            Err(Error::Api(AppError::Forbidden(_) | AppError::NotFound(_))) => {}
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
        total: Total::unknown(),
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
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    query: ReadQuery,
) -> Result<Envelope<Page<ProjectRef>>> {
    let a = auth(state, runtime, user).await?;
    if !a.owner {
        return Err(AppError::Forbidden("owner required".into()).into());
    }
    let (snapshot, offset) = snapshot(state, runtime, &a, &query, Resource::Projects).await?;
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
pub(super) async fn overview(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    scope: Scope,
) -> Result<Envelope<Overview>> {
    let a = auth(state, runtime, user).await?;
    let (ids, _) = membership(state, runtime, &a, &scope, Resource::Sessions).await?;
    let mut projects = BTreeMap::new();
    for id in ids.iter().take(8) {
        match session(state, runtime, user, *id, &scope).await {
            Ok(value) => {
                projects.insert(value.summary.project.id, value.summary.project);
            }
            Err(Error::Api(AppError::Forbidden(_) | AppError::NotFound(_))) => {}
            Err(e) => return Err(e),
        }
    }
    fence(state, runtime, user, &a).await?;
    let data = Overview {
        counts: Counts {
            running: Total::unknown(),
            ready: Total::unknown(),
            needs_you: Total::unknown(),
            recovering: Total::unknown(),
        },
        attention: vec![],
        in_motion: projects
            .into_values()
            .map(|project| InMotion {
                project,
                running: Total::unknown(),
                next_constraint: Some(Constraint {
                    kind: "unknown".into(),
                    explanation: "Task constraints are not connected yet".into(),
                }),
                last_progress_at: None,
            })
            .collect(),
        recent_outcomes: vec![],
    };
    envelope(
        state,
        scope,
        data,
        Some("Task readiness, decisions, and result summaries are not connected yet"),
    )
}
