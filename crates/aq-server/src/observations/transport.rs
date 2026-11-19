//! Central observation store allocation. Node identity is read from the common
//! execution registration; no legacy/native execution grant is manufactured.
use super::*;
use aq_observation_admission::{self as contract, Access, Owner, Receipt, Reply, Request, Status};
fn digest<T: Serialize>(body: &T) -> Result<Digest> {
    Ok(Digest::of(
        &serde_json::to_vec(body).map_err(|_| Error::Invalid)?,
    ))
}
async fn boot(tx: &mut Transaction<'_, Postgres>, node: Uuid) -> Result<Uuid> {
    let identity: Option<Value> =
        sqlx::query_scalar("SELECT state->'identity' FROM execution_nodes WHERE node_id=$1")
            .bind(node)
            .fetch_optional(&mut **tx)
            .await?;
    let identity: aq_worker_protocol::NodeIdentity = parse(identity.ok_or(Error::Refused)?)?;
    if identity.node_id != node || identity.boot_id.is_nil() {
        return Err(Error::Refused);
    }
    Ok(identity.boot_id)
}
async fn load(tx: &mut Transaction<'_, Postgres>, node: Uuid) -> Result<Option<Owner>> {
    let row = sqlx::query(
        "SELECT state,state_digest FROM observation_transport_owners WHERE node_id=$1 FOR UPDATE",
    )
    .bind(node)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| {
        let owner: Owner = parse(row.get("state"))?;
        owner.validate().map_err(|_| Error::Invalid)?;
        if owner.marker.node_id != node
            || digest(&owner)?.as_str() != row.get::<String, _>("state_digest")
        {
            return Err(Error::Invalid);
        }
        Ok(owner)
    })
    .transpose()
}
/// Called only after current node authentication; revocation and incarnation are
/// checked again under the canonical node lock before returning even a replay.
pub async fn mutate(
    state: &AppState,
    authenticated_node: Uuid,
    request: &Request,
) -> Result<Reply> {
    mutate_subject(state, NodeSubject::Trusted(authenticated_node), request).await
}
pub async fn mutate_authenticated(
    state: &AppState,
    context: &crate::worker_api::AuthenticatedNode,
    request: &Request,
) -> Result<Reply> {
    mutate_subject(state, NodeSubject::Authenticated(context), request).await
}
async fn mutate_subject(
    state: &AppState,
    subject: NodeSubject<'_>,
    request: &Request,
) -> Result<Reply> {
    let authenticated_node = subject.id();
    request.validate().map_err(|_| Error::Invalid)?;
    if request.node_id != authenticated_node {
        return Err(Error::Refused);
    }
    let mut tx = state.store.observation_pool.begin().await?;
    subject.lock(&mut tx).await?;
    let boot = boot(&mut tx, authenticated_node).await?;
    sqlx::query("INSERT INTO observation_nodes(node_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(authenticated_node)
        .execute(&mut *tx)
        .await?;
    let ledger=sqlx::query("SELECT through_sequence,event_count,source_count FROM observation_nodes WHERE node_id=$1 FOR UPDATE").bind(authenticated_node).fetch_one(&mut *tx).await?;
    let current = load(&mut tx, authenticated_node).await?;
    if current.is_none()
        && (ledger.get::<i64, _>("event_count") != 0 || ledger.get::<i32, _>("source_count") != 0)
    {
        return Err(Error::Conflict);
    }
    let old=sqlx::query("SELECT request,receipt FROM observation_transport_receipts WHERE node_id=$1 AND operation_id=$2").bind(authenticated_node).bind(request.operation_id).fetch_optional(&mut *tx).await?;
    let previous = old
        .map(|row| {
            let saved: Request = parse(row.get("request"))?;
            if &saved != request {
                return Err(Error::Conflict);
            }
            let receipt: Receipt = parse(row.get("receipt"))?;
            receipt
                .validate_against(&saved)
                .map_err(|_| Error::Invalid)?;
            Ok(receipt)
        })
        .transpose()?;
    let claimed_boot = match request.operation {
        contract::Operation::Prepare { boot_id, .. } => Some(boot_id),
        contract::Operation::AdvanceBoot { new_boot_id, .. } => Some(new_boot_id),
        _ => None,
    };
    let boot_seen:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM observation_transport_receipts WHERE node_id=$1 AND claimed_boot_id=$2)").bind(authenticated_node).bind(claimed_boot).fetch_one(&mut *tx).await?;
    let access = Access {
        node_id: authenticated_node,
        boot_id: boot,
        accepted_through: ledger.get::<i64, _>("through_sequence") as u64,
        max_pending_distance: contract::MAX_PENDING_DISTANCE,
        new_boot_previously_owned: boot_seen,
    };
    let changed = contract::apply(
        current.as_ref(),
        previous.as_ref(),
        request,
        &access,
        Some(Uuid::new_v4()),
    )
    .map_err(|_| Error::Conflict)?;
    if changed.changed {
        let owner = &changed.reply.current_owner;
        if current.is_none() {
            sqlx::query("INSERT INTO observation_transport_owners(node_id,owner_id,state,state_digest) VALUES($1,$2,$3,$4)").bind(authenticated_node).bind(owner.marker.owner_id).bind(value(owner)?).bind(digest(owner)?.as_str()).execute(&mut *tx).await?;
        } else {
            sqlx::query(
                "UPDATE observation_transport_owners SET state=$2,state_digest=$3 WHERE node_id=$1",
            )
            .bind(authenticated_node)
            .bind(value(owner)?)
            .bind(digest(owner)?.as_str())
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("INSERT INTO observation_transport_receipts(node_id,operation_id,owner_revision,request,receipt,claimed_boot_id) VALUES($1,$2,$3,$4,$5,$6)").bind(authenticated_node).bind(request.operation_id).bind(owner.revision as i64).bind(value(request)?).bind(value(&changed.reply.receipt)?).bind(claimed_boot).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(changed.reply)
}
pub async fn status(state: &AppState, authenticated_node: Uuid) -> Result<Status> {
    status_subject(state, NodeSubject::Trusted(authenticated_node)).await
}
pub async fn status_authenticated(
    state: &AppState,
    context: &crate::worker_api::AuthenticatedNode,
) -> Result<Status> {
    status_subject(state, NodeSubject::Authenticated(context)).await
}
async fn status_subject(state: &AppState, subject: NodeSubject<'_>) -> Result<Status> {
    let authenticated_node = subject.id();
    let mut tx = state.store.observation_pool.begin().await?;
    subject.lock(&mut tx).await?;
    let boot = boot(&mut tx, authenticated_node).await?;
    let ledger:Option<(i64,i64,i32)>=sqlx::query_as("SELECT through_sequence,event_count,source_count FROM observation_nodes WHERE node_id=$1 FOR UPDATE").bind(authenticated_node).fetch_optional(&mut *tx).await?;
    let owner = load(&mut tx, authenticated_node).await?;
    let (through, events, sources) = ledger.unwrap_or((0, 0, 0));
    if owner.is_none() && (events != 0 || sources != 0) {
        return Err(Error::Conflict);
    }
    let status = Status {
        version: contract::VERSION,
        node_id: authenticated_node,
        admitted_node_boot_id: boot,
        owner,
        accepted_through: through as u64,
    };
    status.validate().map_err(|_| Error::Invalid)?;
    tx.commit().await?;
    Ok(status)
}

/// Source/ingress barrier, called with the common node and ledger locks held.
/// Only new source enrollment requires the exact currently admitted boot. Late
/// facts from retained sources stay observation-only across a boot transition.
pub(super) async fn require_active_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    new_source_boot: Option<Uuid>,
) -> Result<()> {
    let owner = load(tx, node).await?.ok_or(Error::Refused)?;
    if owner.phase != contract::Phase::Active {
        return Err(Error::Refused);
    }
    if let Some(source_boot) = new_source_boot {
        let registered = boot(tx, node).await?;
        if owner.current_boot_id != source_boot || registered != source_boot {
            return Err(Error::Refused);
        }
    }
    Ok(())
}
