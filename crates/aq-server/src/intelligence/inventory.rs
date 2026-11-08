use super::*;
use aq_selection::{ArtifactRef, CapabilitySnapshot, CredentialBinding, ExecutionTarget};
use axum::http::HeaderMap;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledCredential {
    pub profile: ArtifactRef,
    pub credential_revision: u64,
    pub available: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryReport {
    pub operation_id: Uuid,
    pub boot_id: Uuid,
    pub sequence: u64,
    pub ttl_ms: u64,
    pub qualifications: Vec<ArtifactRef>,
    pub credentials: Vec<InstalledCredential>,
    /// SBX25 measured disk accounting; absent from older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<crate::node_storage::StorageReport>,
}
pub(super) async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<InventoryReport>,
) -> Result<Json<Value>> {
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let node = auth.node();
    if request.operation_id.is_nil()
        || request.boot_id.is_nil()
        || request.sequence == 0
        || request.sequence > i64::MAX as u64
        || !(1000..=120_000).contains(&request.ttl_ms)
        || request.qualifications.len() > 16
        || request.credentials.len() > 32
    {
        return Err(bad("invalid inventory identity/bounds"));
    }
    if let Some(storage) = &request.storage {
        storage.validate()?;
    }
    let payload = aq_selection::digest(&request).map_err(|_| bad("inventory digest"))?;
    // Observe private material outside SQL locks. Only a server-configured
    // delivery adapter on the exact admitted native runtime is considered.
    let candidate: Option<crate::profiled_execution::registration::NativeRegistration> =
        sqlx::query_scalar::<_, Option<Value>>(
            "SELECT state->'native' FROM execution_nodes WHERE node_id=$1",
        )
        .bind(node.id)
        .fetch_optional(&state.store.pool)
        .await
        .map_err(db)?
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok());
    let delivery = candidate
        .as_ref()
        .filter(|c| {
            c.request.identity.node_id == node.id && c.request.identity.boot_id == request.boot_id
        })
        .and_then(|c| {
            state
                .config
                .profiled_execution
                .as_deref()?
                .qualified(&c.request)
                .ok()?
                .material_delivery
                .clone()
        });
    let observed = now(&state)?;
    let observed_expires = observed
        .checked_add(request.ttl_ms)
        .ok_or_else(|| bad("inventory expiry overflow"))?;
    let preparations = if let Some(delivery) = delivery.clone() {
        use std::sync::{Arc, OnceLock};
        static READS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
        let permit = READS
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| conflict("material observation capacity exhausted"))?;
        let registry = loaded(&state)?.clone();
        let node_id = node.id;
        let boot = request.boot_id;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                crate::profiled_execution::material::provisioning_bindings(
                    &registry,
                    node_id,
                    boot,
                    &delivery,
                    observed,
                    observed_expires,
                    None,
                )
            }),
        )
        .await
        .map_err(|_| conflict("material observation timed out"))?
        .map_err(|_| AppError::Internal)?
    } else {
        vec![]
    };
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    crate::worker_api::lock_current_node(&mut tx, &auth).await?;
    if delivery.is_some() {
        let actual = crate::profiled_execution::registration::current_runtime_locked(
            &state,
            &mut tx,
            node.id,
            request.boot_id,
        )
        .await?;
        if candidate.as_ref().is_none_or(|c| {
            c.request.runtime != actual.runtime || c.qualification != actual.qualification
        }) {
            return Err(conflict(
                "native runtime changed during material observation",
            ));
        }
        if now(&state)? >= observed_expires {
            return Err(conflict("material observation expired"));
        }
    }
    let current: Value =
        sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1 FOR SHARE")
            .bind(node.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or_else(|| conflict("execution boot must be registered"))?;
    if current["identity"]["boot_id"] != request.boot_id.to_string() {
        return Err(conflict("inventory boot is stale"));
    }
    if let Some(row)=sqlx::query("SELECT kind,payload_digest,response FROM execution_receipts WHERE node_id=$1 AND operation_id=$2").bind(node.id).bind(request.operation_id).fetch_optional(&mut *tx).await.map_err(db)? {
        if row.get::<String,_>("kind")!="intelligence_inventory" || row.get::<String,_>("payload_digest")!=payload.as_str(){return Err(conflict("inventory operation ID conflict"));}
        return Ok(Json(row.get("response")));
    }
    let loaded = loaded(&state)?;
    registry::check_current(&state, &mut tx).await?;
    let old = sqlx::query(
        "SELECT boot_id,sequence,revision FROM intelligence_node_snapshots WHERE node_id=$1",
    )
    .bind(node.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    if old.as_ref().is_some_and(|row| {
        row.get::<Uuid, _>("boot_id") == request.boot_id
            && row.get::<i64, _>("sequence") as u64 >= request.sequence
    }) {
        return Err(conflict("inventory sequence must increase"));
    }
    let revision = old
        .as_ref()
        .map(|r| r.get::<i64, _>("revision") as u64)
        .unwrap_or(0)
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(AppError::Internal)?;
    let current_time = now(&state)?;
    if delivery.is_some() && (current_time < observed || current_time >= observed_expires) {
        return Err(conflict(
            "material observation expired or clock moved backward",
        ));
    }
    let time = if delivery.is_some() {
        observed
    } else {
        current_time
    };
    let expires = time
        .checked_add(request.ttl_ms)
        .ok_or_else(|| bad("inventory expiry overflow"))?;
    let mut harnesses = Vec::new();
    for qualification in request.qualifications {
        if harnesses
            .iter()
            .any(|h: &aq_selection::InstalledHarness| h.qualification == qualification)
        {
            return Err(bad("duplicate harness qualification"));
        }
        let harness = loaded
            .config
            .qualified_harnesses
            .iter()
            .find(|h| h.qualification == qualification)
            .ok_or_else(|| bad("unrecognized qualified harness"))?;
        harnesses.push(harness.clone());
    }
    let mut credential_bindings = Vec::new();
    let enrolled = aq_machine_control::enrolled(&mut tx, node.id)
        .await
        .map_err(|_| AppError::Internal)?;
    for presence in request.credentials {
        if credential_bindings
            .iter()
            .any(|c: &CredentialBinding| c.credential_profile == presence.profile)
        {
            return Err(bad("duplicate installed credential profile"));
        }
        let configured = loaded
            .config
            .credentials
            .iter()
            .find(|c| {
                c.credential.profile == presence.profile
                    && c.credential.revision == presence.credential_revision
            })
            .ok_or_else(|| bad("unknown exact credential revision"))?;
        if !configured.admits(node.id, enrolled) {
            return Err(AppError::Forbidden(
                "credential not authorized for node".into(),
            ));
        }
        if presence.available && configured.delivery_mode.is_some() {
            return Err(bad(
                "server material installation requires an exact verified delivery receipt",
            ));
        }
        credential_bindings.push(CredentialBinding {
            credential_revision: presence.credential_revision,
            credential_profile: presence.profile,
            available: presence.available,
            preparation: None,
            observed_at_ms: time,
            expires_at_ms: expires,
        });
    }
    for preparation in preparations {
        if let Some(binding) = credential_bindings.iter_mut().find(|c| {
            c.credential_profile == preparation.credential_profile
                && c.credential_revision == preparation.credential_revision
        }) {
            if !binding.available {
                *binding = preparation;
            }
        } else {
            credential_bindings.push(preparation);
        }
    }
    let snapshot = CapabilitySnapshot {
        backend_kind: None,
        material_delivery: delivery.clone(),
        target: ExecutionTarget::Worker {
            node_id: node.id,
            boot_id: request.boot_id,
        },
        revision,
        observed_at_ms: time,
        expires_at_ms: expires,
        harnesses,
        credential_bindings,
    };
    let value = serde_json::to_value(snapshot).map_err(|_| AppError::Internal)?;
    registry::machine_quota(&mut tx, node.id, &value).await?;
    if delivery.is_some() {
        let current_time = now(&state)?;
        if current_time < observed || current_time >= observed_expires {
            return Err(conflict(
                "material observation expired or clock moved backward",
            ));
        }
    }

    sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,$3,$4,$5) ON CONFLICT(node_id) DO UPDATE SET boot_id=excluded.boot_id,sequence=excluded.sequence,revision=excluded.revision,snapshot=excluded.snapshot").bind(node.id).bind(request.boot_id).bind(request.sequence as i64).bind(revision as i64).bind(&value).execute(&mut *tx).await.map_err(db)?;
    crate::profiled_execution::presence::record_legacy_inventory_locked(
        &mut tx,
        node.id,
        request.boot_id,
        revision,
        &value,
    )
    .await?;
    if let Some(storage) = &request.storage {
        crate::node_storage::record(&mut tx, &node.id.to_string(), Some(node.id), storage, time)
            .await?;
    }
    sqlx::query("INSERT INTO execution_receipts(node_id,operation_id,kind,payload_digest,response) VALUES($1,$2,'intelligence_inventory',$3,$4)").bind(node.id).bind(request.operation_id).bind(payload.as_str()).bind(&value).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(Json(value))
}
