//! Operator-approved Option A: native history keeps its original boot. Only the
//! delegated incarnation and current permit change on readoption. Renewal may
//! extend the hot grant lease without collecting fresh native observations.
use super::{handoff, runtime::*};
use crate::{
    AppState,
    error::AppError,
    worker_api::{self, AuthenticatedNode},
};
use aq_admission_sandbox::sandbox::*;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use std::result::Result;
use uuid::Uuid;

pub(crate) fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v2/worker/readopt", post(readopt))
        .layer(DefaultBodyLimit::max(32_768))
        .with_state(state.control_view())
}
fn response(document: SandboxDocument) -> Response {
    let mut reply = Json(document).into_response();
    reply
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    reply
}
async fn begin(state: &AppState) -> Result<Transaction<'_, Postgres>, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    Ok(tx)
}
async fn commit(tx: Transaction<'_, Postgres>) -> Result<(), AppError> {
    tx.commit().await.map_err(|_| {
        AppError::UpdatesUnavailable(
            "incarnation commit outcome unknown; preserve exact operation intent".into(),
        )
    })
}
pub(crate) async fn registered(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
) -> Result<Option<Registration>, AppError> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT registration FROM delegated_node_boots WHERE node_id=$1")
            .bind(node)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
    value.map(parse).transpose()
}

/// All advertised template digests and capabilities at issuance are conservatively
/// pinned. A changed or missing item cannot silently weaken a grant requirement.
fn compatible(original: &Registration, next: &Registration) -> Result<(), AppError> {
    original.validate().map_err(|_| refused())?;
    next.validate().map_err(|_| refused())?;
    let mut a = json(original)?;
    let mut b = json(next)?;
    for v in [&mut a, &mut b] {
        v["registration"]
            .as_object_mut()
            .ok_or_else(refused)?
            .remove("operation_id");
        v["registration"]["identity"]
            .as_object_mut()
            .ok_or_else(refused)?
            .remove("boot_id");
        // Capacity and estimates do not grant authority; kind, capabilities,
        // templates and exact runtime remain pinned.
        let backend = v["backend"].as_object_mut().ok_or_else(refused)?;
        backend.remove("max_concurrent_sandboxes");
        backend.remove("spend_ceiling");
        for template in backend
            .get_mut("templates")
            .and_then(|v| v.as_array_mut())
            .ok_or_else(refused)?
        {
            template
                .as_object_mut()
                .ok_or_else(refused)?
                .remove("estimated_cold_start_ms");
            template
                .as_object_mut()
                .ok_or_else(refused)?
                .remove("estimated_cold_start_cost");
        }
    }
    if a != b {
        return Err(refused());
    }
    Ok(())
}
pub(crate) async fn check_rebound(
    tx: &mut Transaction<'_, Postgres>,
    current: &Current,
) -> Result<(), AppError> {
    let Some(origin) = &current.authority else {
        return Ok(());
    };
    let aq_session_protocol::SessionIdentity::Remote { boot_id, .. } = origin.grant.binding.session
    else {
        return Err(refused());
    };
    if boot_id != current.record.binding.node.boot_id {
        let registered = registered(tx, current.record.binding.node.node_id)
            .await?
            .ok_or_else(refused)?;
        compatible(
            current.registration.as_ref().ok_or_else(refused)?,
            &registered,
        )?;
    }
    Ok(())
}
async fn remember_bounded(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    op: Uuid,
    kind: &str,
    request: &impl Serialize,
    evidence: &impl Serialize,
) -> Result<(), AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-delegated-history-quota',0))")
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    let (total, local): (i64, i64) = sqlx::query_as("SELECT COALESCE(sum(octet_length(evidence::text)+512),0)::BIGINT,COALESCE(sum(octet_length(evidence::text)+512) FILTER(WHERE node_id=$1),0)::BIGINT FROM delegated_operations")
        .bind(node).fetch_one(&mut **tx).await.map_err(db)?;
    let size = serde_json::to_vec(evidence).map_err(|_| refused())?.len() as i64 + 512;
    if size > 1_048_576 || total + size > 67_108_864 || local + size > 4_194_304 {
        return Err(refused());
    }
    handoff::remember(tx, node, op, kind, request, evidence).await
}

pub(crate) async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(document): Json<SandboxDocument>,
) -> Result<Response, AppError> {
    let auth = worker_api::authenticate_context(&state, &headers).await?;
    let SandboxDocument::Register(q) = document else {
        return Err(refused());
    };
    q.validate().map_err(|_| refused())?;
    key(&state)?;
    if q.registration.identity.node_id != auth.id()
        || serde_json::to_vec(&q).map_err(|_| refused())?.len() > 16_384
    {
        return Err(refused());
    }
    let mut tx = begin(&state).await?;
    worker_api::lock_current_node(&mut tx, &auth).await?;
    let old = registered(&mut tx, auth.id()).await?;
    if let Some(replay) = handoff::replay(
        &mut tx,
        auth.id(),
        q.registration.operation_id,
        "registration",
        &q,
    )
    .await?
    {
        if old
            .as_ref()
            .is_none_or(|old| old.registration.identity != q.registration.identity)
        {
            return Err(refused());
        }
        let reply: RegistrationResponse = parse(replay["response"].clone())?;
        reply.validate_for(&q).map_err(|_| refused())?;
        commit(tx).await?;
        return Ok(response(SandboxDocument::RegistrationResponse(reply)));
    }
    // A previously registered incarnation cannot be resurrected using a new op.
    let seen: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM delegated_operations WHERE node_id=$1 AND kind='registration' AND evidence#>>'{response,node,boot_id}'=$2)")
        .bind(auth.id()).bind(q.registration.identity.boot_id.to_string()).fetch_one(&mut *tx).await.map_err(db)?;
    if seen
        && old
            .as_ref()
            .is_none_or(|old| old.registration.identity != q.registration.identity)
    {
        return Err(refused());
    }
    if let Some(old) = &old
        && old.registration.identity == q.registration.identity
    {
        let mut old = json(old)?;
        let mut new = json(&q)?;
        old["registration"]
            .as_object_mut()
            .ok_or_else(refused)?
            .remove("operation_id");
        new["registration"]
            .as_object_mut()
            .ok_or_else(refused)?
            .remove("operation_id");
        if old != new {
            return Err(refused());
        }
    }
    let native_boot: Option<Uuid> = sqlx::query_scalar(
        "SELECT (state#>>'{identity,boot_id}')::uuid FROM execution_nodes WHERE node_id=$1",
    )
    .bind(auth.id())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    // The native boot predates v2 registration history on upgraded servers.
    // Treat it as used too: a fresh operation ID must not resurrect that boot.
    if native_boot == Some(q.registration.identity.boot_id)
        && old
            .as_ref()
            .is_some_and(|old| old.registration.identity != q.registration.identity)
    {
        return Err(refused());
    }
    if native_boot.is_none() || native_boot == Some(q.registration.identity.boot_id) {
        crate::profiled_execution::registration::register_locked(
            &state,
            &mut tx,
            &auth,
            &parse(json(&q.registration)?)?,
        )
        .await?;
    }
    let active = if old.is_some() {
        boot(&mut tx, auth.id()).await?
    } else {
        native_boot.unwrap_or(q.registration.identity.boot_id)
    };
    sqlx::query("INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3) ON CONFLICT(node_id) DO UPDATE SET registration=excluded.registration")
        .bind(auth.id()).bind(active).bind(json(&q)?).execute(&mut *tx).await.map_err(db)?;
    let reply = RegistrationResponse {
        schema_version: 2,
        operation_id: q.registration.operation_id,
        node: q.registration.identity.clone(),
        compatibility: CompatibilityReply::Compatible,
    };
    reply.validate_for(&q).map_err(|_| refused())?;
    remember_bounded(
        &mut tx,
        auth.id(),
        q.registration.operation_id,
        "registration",
        &q,
        &serde_json::json!({"response":reply}),
    )
    .await?;
    commit(tx).await?;
    Ok(response(SandboxDocument::RegistrationResponse(reply)))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Adoption {
    before: Vec<Current>,
    after: Vec<Current>,
    refused: Vec<ReadoptionResult>,
}
impl Adoption {
    fn response(
        &self,
        state: &AppState,
        q: &ReadoptionRequest,
        time: u64,
    ) -> Result<ReadoptionResponse, AppError> {
        let mut results = self.refused.clone();
        for current in &self.after {
            results.push(ReadoptionResult::Rebound {
                permit: current
                    .record
                    .delivery(key(state)?)
                    .map_err(|_| refused())?,
            });
        }
        // Preserve the request's order for consumers and exact replay.
        results.sort_by_key(|result| {
            q.sandboxes.iter().position(|s| {
                s.sandbox_id
                    == match result {
                        ReadoptionResult::Rebound { permit } => permit.binding.sandbox_id,
                        ReadoptionResult::Refused { sandbox_id, .. } => *sandbox_id,
                    }
            })
        });
        let reply = ReadoptionResponse {
            schema_version: 1,
            operation_id: q.operation_id,
            previous: q.previous.clone(),
            current: q.current.clone(),
            results,
        };
        let previous = self
            .before
            .iter()
            .map(|c| c.record.delivery(key(state)?).map_err(|_| refused()))
            .collect::<Result<Vec<_>, AppError>>()?;
        reply
            .validate_for(q, &previous, time)
            .map_err(|_| refused())?;
        Ok(reply)
    }
}
async fn readopt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(document): Json<SandboxDocument>,
) -> Result<Response, AppError> {
    let auth = worker_api::authenticate_context(&state, &headers).await?;
    let SandboxDocument::ReadoptionRequest(q) = document else {
        return Err(refused());
    };
    q.validate().map_err(|_| refused())?;
    if q.current.node_id != auth.id() {
        return Err(refused());
    }
    let mut tx = begin(&state).await?;
    worker_api::lock_current_node(&mut tx, &auth).await?;
    let registration = registered(&mut tx, auth.id()).await?.ok_or_else(refused)?;
    if registration.registration.identity != q.current {
        return Err(refused());
    }
    let active = boot(&mut tx, auth.id()).await?;
    // ReportResult is allowed for both live and ended execution grants. Rebinding
    // restores only the operations the current phase permits, never ended renewal.
    let retained = if let Some(value) =
        handoff::replay(&mut tx, auth.id(), q.operation_id, "readoption", &q).await?
    {
        if active != q.current.boot_id {
            return Err(refused());
        }
        let retained: Adoption = parse(value)?;
        for old in &retained.after {
            lock_owner(&mut tx, old).await?;
            let current = load(&mut tx, old.record.permit_id).await?;
            if json(&current)? != json(old)? {
                return Err(refused());
            }
            let context = context(&mut tx, &current).await?;
            check_rebound(&mut tx, &current).await?;
            current
                .record
                .authorize(&context, RunnerOperation::ReportResult, now(&state)?)
                .map_err(|_| refused())?;
        }
        retained
    } else {
        if active != q.previous.boot_id {
            return Err(refused());
        }
        let mut retained = Adoption {
            before: vec![],
            after: vec![],
            refused: vec![],
        };
        for sandbox in &q.sandboxes {
            let raw: Option<serde_json::Value> = sqlx::query_scalar("SELECT c.state FROM delegated_permit_current c JOIN delegated_permits p USING(permit_id) WHERE c.permit_id=$1 AND p.node_id=$2 AND NOT p.revoked")
                .bind(sandbox.permit_id).bind(auth.id()).fetch_optional(&mut *tx).await.map_err(db)?;
            let Some(raw) = raw else {
                retained.refused.push(ReadoptionResult::Refused {
                    sandbox_id: sandbox.sandbox_id,
                    code: ReadoptionRefusal::UnknownSandbox,
                });
                continue;
            };
            let old: Current = parse(raw)?;
            lock_owner(&mut tx, &old).await?;
            let mut current = load(&mut tx, sandbox.permit_id).await?;
            let matches = current.record.binding.node == q.previous
                && current.record.binding.attempt_id == sandbox.attempt_id
                && current.record.binding.sandbox_id == sandbox.sandbox_id
                && current.record.binding.grant_generation == sandbox.grant_generation
                && current.record.scope == PermitScope::Execution;
            if !matches {
                retained.refused.push(ReadoptionResult::Refused {
                    sandbox_id: sandbox.sandbox_id,
                    code: ReadoptionRefusal::UnknownSandbox,
                });
                continue;
            }
            let valid = async {
                compatible(
                    current.registration.as_ref().ok_or_else(refused)?,
                    &registration,
                )?;
                let context = context(&mut tx, &current).await?;
                current
                    .record
                    .authorize(&context, RunnerOperation::ReportResult, now(&state)?)
                    .map_err(|_| refused())
            }
            .await;
            if valid.is_err() {
                retained.refused.push(ReadoptionResult::Refused {
                    sandbox_id: sandbox.sandbox_id,
                    code: ReadoptionRefusal::NoCurrentGrant,
                });
                continue;
            }
            retained.before.push(current.clone());
            current
                .record
                .rebind(key(&state)?, q.current.boot_id)
                .map_err(|_| refused())?;
            // Revision is retained: the owner readoption response has no revision
            // field. Rebinding extends neither the lease nor the hard deadline.
            save(&mut tx, &current).await?;
            retained.after.push(current);
        }
        sqlx::query("UPDATE delegated_node_boots SET boot_id=$2 WHERE node_id=$1")
            .bind(auth.id())
            .bind(q.current.boot_id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        remember_bounded(
            &mut tx,
            auth.id(),
            q.operation_id,
            "readoption",
            &q,
            &retained,
        )
        .await?;
        retained
    };
    for current in &retained.after {
        let context = context(&mut tx, current).await?;
        current
            .record
            .authorize(&context, RunnerOperation::ReportResult, now(&state)?)
            .map_err(|_| refused())?;
    }
    // Even omitted/refused sandboxes are fenced by the node incarnation switch.
    let reply = retained.response(&state, &q, now(&state)?)?;
    commit(tx).await?;
    Ok(response(SandboxDocument::ReadoptionResponse(reply)))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Renewal {
    current: Current,
}
/// Pending registration returns awaiting without changing any deadline. After
/// re-adoption Option A uses the live grant/cancellation/hard deadline only.
pub(crate) async fn renew_after_reboot(
    state: &AppState,
    auth: &AuthenticatedNode,
    q: &RunnerRenewalRequest,
    nonce: Uuid,
) -> Result<Option<RunnerRenewalResponse>, AppError> {
    q.validate().map_err(|_| refused())?;
    if nonce.is_nil() {
        return Err(refused());
    }
    let mut tx = begin(state).await?;
    worker_api::lock_current_node(&mut tx, auth).await?;
    let old = &auth.delegated().ok_or_else(refused)?.current;
    if q.permit.permit_id != old.record.permit_id
        || q.permit.binding != old.record.binding
        || json(&Some(&q.authority))? != json(&old.authority)?
    {
        return Err(refused());
    }
    old.record
        .verify_credential(q.permit.credential.expose())
        .map_err(|_| refused())?;
    let Some(registration) = registered(&mut tx, auth.id()).await? else {
        return Ok(None);
    };
    let pending = registration.registration.identity != old.record.binding.node;
    let aq_session_protocol::SessionIdentity::Remote { boot_id, .. } =
        q.authority.grant.binding.session
    else {
        return Err(refused());
    };
    let rebound = boot_id != old.record.binding.node.boot_id;
    if !pending && !rebound {
        return Ok(None);
    }
    let replay = handoff::replay(&mut tx, auth.id(), q.operation_id, "renewal", q)
        .await?
        .map(parse::<Renewal>)
        .transpose()?;
    if let Some(replay) = &replay {
        if json(&replay.current)? != json(old)? {
            return Err(refused());
        }
    } else if q.revision != old.revision
        || json(&q.permit)? != json(&old.record.delivery(key(state)?).map_err(|_| refused())?)?
    {
        return Err(refused());
    }
    let context = context(&mut tx, old).await?;
    let time = now(state)?;
    old.record
        .authorize(&context, RunnerOperation::Renew, time)
        .map_err(|_| refused())?;
    if pending {
        let reply = RunnerRenewalResponse {
            schema_version: 1,
            revision: q.revision,
            reply: RenewalReply::AwaitingNodeReadoption {
                operation_id: q.operation_id,
                binding: q.permit.binding.clone(),
            },
            delivery: None,
        };
        reply.validate().map_err(|_| refused())?;
        commit(tx).await?;
        return Ok(Some(reply));
    }
    compatible(
        old.registration.as_ref().ok_or_else(refused)?,
        &registration,
    )?;
    let next = if let Some(replay) = replay {
        replay.current
    } else {
        if context.grant_expires_at_ms != old.record.expires_at_ms {
            return Err(refused());
        }
        let expiry = time
            .checked_add(120_000)
            .ok_or_else(refused)?
            .min(old.record.hard_deadline_ms);
        if expiry <= old.record.expires_at_ms {
            return Err(refused());
        }
        let mut next = old.clone();
        next.record.issued_at_ms = time;
        next.record.expires_at_ms = expiry;
        next.revision = next.revision.checked_add(1).ok_or_else(refused)?;
        sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{grant,expires_at_ms}',$2) WHERE attempt_id=$1")
            .bind(old.record.binding.attempt_id).bind(json(&expiry)?).execute(&mut *tx).await.map_err(db)?;
        save(&mut tx, &next).await?;
        remember_bounded(
            &mut tx,
            auth.id(),
            q.operation_id,
            "renewal",
            q,
            &Renewal {
                current: next.clone(),
            },
        )
        .await?;
        next
    };
    let time = now(state)?;
    // Check the prior lease again after all waits, not just the newly extended one.
    old.record
        .authorize(&context, RunnerOperation::Renew, time)
        .map_err(|_| refused())?;
    let permit = next.record.delivery(key(state)?).map_err(|_| refused())?;
    let reply = RunnerRenewalResponse {
        schema_version: 1,
        revision: next.revision,
        delivery: Some(aq_admission_sandbox::control::DeliveryStamp {
            nonce,
            clock: aq_admission_sandbox::transport::DeliveryClock::new_interactive_session(
                time,
                permit.expires_at_ms,
            )
            .map_err(|_| refused())?,
        }),
        reply: RenewalReply::Renewed {
            operation_id: q.operation_id,
            permit,
        },
    };
    reply.validate().map_err(|_| refused())?;
    commit(tx).await?;
    Ok(Some(reply))
}
