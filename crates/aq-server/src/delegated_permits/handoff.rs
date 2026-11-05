//! Permit issuance is committed by the existing accept and grant owners.
use super::{PermitRecord, runtime::*};
use crate::{AppState, error::AppError, worker_api::AuthenticatedNode};
use aq_admission_sandbox::sandbox::*;
use axum::http::{HeaderMap, HeaderValue};
use serde::Serialize;
use sqlx::{Postgres, Row, Transaction};
use std::result::Result;
use uuid::Uuid;

pub fn header<T: serde::de::DeserializeOwned>(
    headers: &HeaderMap,
    name: &str,
) -> Result<Option<T>, AppError> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() || value.as_bytes().len() > 8192 {
        return Err(refused());
    }
    serde_json::from_slice(value.as_bytes())
        .map(Some)
        .map_err(|_| refused())
}
pub(crate) async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    operation: Uuid,
    kind: &str,
    request: &impl Serialize,
) -> Result<Option<serde_json::Value>, AppError> {
    let row=sqlx::query("SELECT kind,request_digest,evidence FROM delegated_operations WHERE node_id=$1 AND operation_id=$2").bind(node).bind(operation).fetch_optional(&mut **tx).await.map_err(db)?;
    row.map(|row| {
        if row.get::<String, _>("kind") != kind
            || row.get::<Vec<u8>, _>("request_digest") != digest(request)?
        {
            return Err(AppError::Conflict("delegated operation ID reused".into()));
        }
        Ok(row.get("evidence"))
    })
    .transpose()
}
pub(crate) async fn remember(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    operation: Uuid,
    kind: &str,
    request: &impl Serialize,
    evidence: &impl Serialize,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO delegated_operations(node_id,operation_id,kind,request_digest,evidence) VALUES($1,$2,$3,$4,$5)")
        .bind(node).bind(operation).bind(kind).bind(digest(request)?.as_slice()).bind(json(evidence)?).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}
pub async fn issue_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &impl Serialize,
    receipt: &impl Serialize,
) -> Result<(), AppError> {
    let Some(q) = &auth.issuance else {
        return Ok(());
    };
    if q.node.node_id != auth.id() {
        return Err(refused());
    }
    let authority = match q.profile {
        PhaseProfile::PhaseV2 => {
            q.validate_accept_v2(&parse(json(request)?)?)
                .map_err(|_| refused())?;
            PreparationAuthority::from_phase_v2(&parse(json(receipt)?)?)
        }
        PhaseProfile::PhaseRefreshV1 => {
            q.validate_accept_refresh(&parse(json(request)?)?)
                .map_err(|_| refused())?;
            PreparationAuthority::from_phase_refresh(&parse(json(receipt)?)?)
        }
    }
    .map_err(|_| refused())?;
    if let Some(old) = replay(tx, auth.id(), q.operation_id, "issuance", q).await? {
        let current: Current = parse(old)?;
        if json(&current.preparation)? != json(&Some(authority))? {
            return Err(refused());
        }
        current
            .preparation
            .as_ref()
            .ok_or_else(refused)?
            .validate_permit(
                &current
                    .record
                    .delivery(key(state)?)
                    .map_err(|_| refused())?,
                RunnerOperation::Prepared,
                now(state)?,
            )
            .map_err(|_| refused())?;
        return Ok(());
    }
    if boot(tx, auth.id()).await? != q.node.boot_id {
        return Err(refused());
    }
    let registration = super::incarnation::registered(tx, auth.id()).await?;
    if registration
        .as_ref()
        .is_some_and(|r| r.registration.identity != q.node)
    {
        return Err(refused());
    }
    let id = Uuid::new_v4();
    let permit = DelegatedPermit {
        schema_version: 1,
        permit_id: id,
        credential: key(state)?.credential(id).map_err(|_| refused())?,
        binding: PermitBinding {
            attempt_id: q.attempt_id,
            grant_generation: q.grant_generation,
            sandbox_id: q.sandbox_id,
            node: q.node.clone(),
            channel_id: Uuid::new_v4(),
        },
        scope: PermitScope::Preparation,
        issued_at_ms: now(state)?,
        expires_at_ms: authority.preparation_deadline_ms,
        hard_deadline_ms: authority.preparation_deadline_ms,
    };
    authority
        .validate_permit(&permit, RunnerOperation::Prepared, now(state)?)
        .map_err(|_| refused())?;
    let record = super::storage::insert_locked(tx, q.operation_id, digest(q)?, &permit)
        .await
        .map_err(|_| refused())?;
    let current = Current {
        record,
        preparation: Some(authority),
        authority: None,
        revision: 1,
        registration,
    };
    save(tx, &current).await?;
    // Bind the attempt's admitted private material to this permit only.
    sqlx::query("UPDATE admission_material_leases SET permit_id=$1 WHERE admitted_attempt_id=$2 AND node_id=$3 AND state='admitted' AND permit_id IS NULL AND lease_id IN (SELECT lease_id FROM profiled_launch_materials WHERE attempt_id=$2)")
        .bind(id)
        .bind(q.attempt_id)
        .bind(q.node.node_id)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    remember(tx, auth.id(), q.operation_id, "issuance", q, &current).await
}
pub async fn upgrade_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &impl Serialize,
    receipt: &impl Serialize,
) -> Result<(), AppError> {
    let Some(q) = &auth.upgrade else {
        return if auth.delegated().is_some() {
            Err(refused())
        } else {
            Ok(())
        };
    };
    let delegate = auth.delegated().ok_or_else(refused)?;
    if super::incarnation::registered(tx, auth.id())
        .await?
        .as_ref()
        .is_some_and(|r| r.registration.identity != q.binding.node)
    {
        return Err(refused());
    }
    if delegate.current.record.permit_id != q.permit_id {
        return Err(refused());
    }
    let authority = match q.profile {
        PhaseProfile::PhaseV2 => {
            q.validate_prepare_v2(&parse(json(request)?)?)
                .map_err(|_| refused())?;
            PhaseExecutionAuthority::from_phase_v2(&parse(json(receipt)?)?)
        }
        PhaseProfile::PhaseRefreshV1 => {
            q.validate_prepare_refresh(&parse(json(request)?)?)
                .map_err(|_| refused())?;
            PhaseExecutionAuthority::from_phase_refresh(&parse(json(receipt)?)?)
        }
    }
    .map_err(|_| refused())?;
    if let Some(old) = replay(tx, auth.id(), q.operation_id, "upgrade", q).await? {
        let current: Current = parse(old)?;
        if json(&current.authority)? != json(&Some(authority))? {
            return Err(refused());
        }
        return Ok(());
    }
    let mut current = load(tx, q.permit_id).await?;
    let mut permit = current
        .record
        .delivery(key(state)?)
        .map_err(|_| refused())?;
    let preparation = current.preparation.as_ref().ok_or_else(refused)?;
    q.validate_for(preparation, &permit, now(state)?)
        .map_err(|_| refused())?;
    permit.scope = PermitScope::Execution;
    permit.issued_at_ms = now(state)?;
    permit.expires_at_ms = authority.grant.expires_at_ms;
    permit.hard_deadline_ms = authority.hard_deadline_ms;
    authority
        .validate_handoff(&permit, now(state)?)
        .map_err(|_| refused())?;
    current.record = PermitRecord::from_issued(&permit).map_err(|_| refused())?;
    current.authority = Some(authority);
    current.revision += 1;
    save(tx, &current).await?;
    remember(tx, auth.id(), q.operation_id, "upgrade", q, &current).await
}
pub async fn response_header(
    state: &AppState,
    auth: &AuthenticatedNode,
    response: &mut axum::response::Response,
) -> Result<(), AppError> {
    let (op, name) = if let Some(q) = &auth.issuance {
        (q.operation_id, "aq-permit-issuance")
    } else if let Some(q) = &auth.upgrade {
        (q.operation_id, "aq-permit-upgrade")
    } else {
        return Ok(());
    };
    let current: Current = parse(
        sqlx::query_scalar(
            "SELECT evidence FROM delegated_operations WHERE node_id=$1 AND operation_id=$2",
        )
        .bind(auth.id())
        .bind(op)
        .fetch_one(&state.store.pool)
        .await
        .map_err(db)?,
    )?;
    let permit = current
        .record
        .delivery(key(state)?)
        .map_err(|_| refused())?;
    let value = if auth.issuance.is_some() {
        json(&PermitIssuanceResponse {
            schema_version: 1,
            operation_id: op,
            authority: current.preparation.ok_or_else(refused)?,
            permit,
        })?
    } else {
        json(&PermitUpgradeResponse {
            schema_version: 1,
            operation_id: op,
            authority: current.authority.ok_or_else(refused)?,
            permit,
        })?
    };
    response.headers_mut().insert(
        name,
        HeaderValue::from_str(&serde_json::to_string(&value).map_err(|_| AppError::Internal)?)
            .map_err(|_| AppError::Internal)?,
    );
    response.headers_mut().insert(
        "aq-permit-revision",
        HeaderValue::from_str(&current.revision.to_string()).map_err(|_| AppError::Internal)?,
    );
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    Ok(())
}
