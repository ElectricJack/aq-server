use super::*;

pub(super) async fn provision_base(
    State(s): State<AppState>,
    _owner: crate::authz::RequireOwner,
    ApiJson(r): ApiJson<ProvisionBase>,
) -> Result<Json<TransferManifest>> {
    if r.operation_id.is_nil() || r.retention_handle.is_nil() {
        return Err(bad("non-nil base operation and handle required"));
    }
    let rt = runtime(&s)?;
    let _bulk = rt.admit_bulk()?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let node: Uuid =
        sqlx::query_scalar("SELECT node_id FROM execution_attempts WHERE attempt_id=$1")
            .bind(r.attempt_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or_else(|| AppError::NotFound("attempt not found".into()))?;
    let a = authority(&mut tx, node, r.attempt_id).await?;
    let b = binding(&a)?.clone();
    let reservation = BaseReservation {
        request: &r,
        state: &s,
        runtime: &rt,
        node,
    };
    if let Some(manifest) = reservation.reserve(&mut tx).await? {
        return Ok(Json(manifest));
    }
    tx.commit().await.map_err(internal)?;
    let manifest = r.export(&rt, a, b).await?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let current = authority(&mut tx, node, r.attempt_id).await?;
    if binding(&current)? != &manifest.source {
        return Err(conflict("base binding changed"));
    }
    sqlx::query("UPDATE source_bases SET manifest=$2::jsonb,reserved_bytes=$4 WHERE attempt_id=$1 AND operation_id=$3").bind(r.attempt_id).bind(serde_json::to_string(&manifest).map_err(internal)?).bind(r.operation_id).bind((manifest.manifest_bytes+manifest.bundle_bytes) as i64).execute(&mut *tx).await.map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(manifest))
}
struct BaseReservation<'a> {
    request: &'a ProvisionBase,
    state: &'a AppState,
    runtime: &'a Runtime,
    node: Uuid,
}
impl BaseReservation<'_> {
    async fn reserve(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<Option<TransferManifest>> {
        let existing=sqlx::query("SELECT operation_id,retention_handle,manifest::text AS manifest FROM source_bases WHERE attempt_id=$1 FOR UPDATE").bind(self.request.attempt_id).fetch_optional(&mut **tx).await.map_err(internal)?;
        let Some(old) = existing else {
            self.insert(tx).await?;
            return Ok(None);
        };
        let previous = (
            old.get::<Uuid, _>("operation_id"),
            old.get::<Uuid, _>("retention_handle"),
        );
        if previous != (self.request.operation_id, self.request.retention_handle) {
            return Err(conflict("base provisioning identity conflict"));
        }
        old.get::<Option<&str>, _>("manifest")
            .map(serde_json::from_str)
            .transpose()
            .map_err(internal)
    }
    async fn insert(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        sqlx::query("SELECT pg_advisory_xact_lock(719247031)")
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
        let used = aq_history_caps::staging_used(tx, (self.state.clock)().timestamp_millis())
            .await
            .map_err(internal)?;
        let reserved = self.runtime.config.max_bundle_bytes + 1024 * 1024;
        if (used as u64).saturating_add(reserved) > self.runtime.config.max_staging_bytes {
            return Err(AppError::PayloadTooLarge(
                "base staging quota exhausted".into(),
            ));
        }
        sqlx::query("INSERT INTO source_bases(attempt_id,node_id,operation_id,retention_handle,reserved_bytes) VALUES($1,$2,$3,$4,$5)").bind(self.request.attempt_id).bind(self.node).bind(self.request.operation_id).bind(self.request.retention_handle).bind(reserved as i64).execute(&mut **tx).await.map_err(internal)?;
        Ok(())
    }
}

impl ProvisionBase {
    async fn export(
        &self,
        rt: &Runtime,
        a: crate::execution::authority::AuthorityView,
        b: SourceBinding,
    ) -> Result<TransferManifest> {
        let request = json!({"protocol_version":1,"request_id":format!("base-export-{}",self.operation_id),"operation":"artifact.export","repository_id":b.repository_id,"operation_id":self.operation_id,"expected_binding_generation":b.binding_generation,"params":{"source":b.base,"base":b.base,"retention_handle":self.retention_handle,"confirmed_prerequisites":[]},"deadline_ms":rt.config.git_deadline_ms});
        let _result = export_base(rt, &request).await?;
        let dir = rt
            .config
            .git_root
            .join("artifacts")
            .join(self.operation_id.to_string());
        let bytes = tokio::fs::read(dir.join("manifest.json"))
            .await
            .map_err(internal)?;
        if bytes.len() > 1024 * 1024 {
            return Err(bad("exported manifest too large"));
        }
        let m: GitManifest = serde_json::from_slice(&bytes).map_err(internal)?;
        let bound = m.matches_repository(&b) && m.source == b.base;
        let artifact = m.transfer_id == self.operation_id.to_string()
            && m.bundle_size <= rt.config.max_bundle_bytes;
        if !bound || !artifact {
            return Err(conflict("exported base binding mismatch"));
        }
        let manifest = TransferManifest {
            transfer_id: self.operation_id,
            role: aq_worker_protocol::SourceRole::Base,
            checkpoint: b.base.clone(),
            attempt: a.attempt,
            resolved_spec_digest: a.execution_spec_digest,
            source: b,
            artifact_id: self.operation_id,
            manifest_digest: digest(&bytes),
            bundle_digest: m.bundle_sha256,
            manifest_bytes: bytes.len() as u64,
            bundle_bytes: m.bundle_size,
        };
        manifest
            .validate()
            .map_err(|_| bad("exported base manifest invalid"))?;
        Ok(manifest)
    }
}
async fn export_base(rt: &Runtime, request: &Value) -> Result<Value> {
    // Private, finite artifact.export mapping; public routes cannot choose operation.
    let exchange = artifact_exchange(rt, request);
    tokio::time::timeout(rt.git_wait(), exchange)
        .await
        .map_err(|_| {
            AppError::UpdatesUnavailable("base export pending; retry same operation".into())
        })?
        .map_err(|e| {
            tracing::warn!(error=%e,"base export uncertain");
            AppError::UpdatesUnavailable("base export pending; retry same operation".into())
        })
}
async fn load_base(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    id: Uuid,
) -> Result<TransferManifest> {
    let a = authority(tx, node, id).await?;
    let row=sqlx::query("SELECT manifest::text AS manifest,profiled_export_operation_id FROM source_bases WHERE attempt_id=$1 AND node_id=$2")
        .bind(id).bind(node).fetch_optional(&mut **tx).await.map_err(internal)?
        .ok_or_else(||AppError::NotFound("base is not provisioned".into()))?;
    if row
        .get::<Option<Uuid>, _>("profiled_export_operation_id")
        .is_some()
    {
        return Err(AppError::Forbidden(
            "native source transfer requires current-credential native API".into(),
        ));
    }
    let body: Option<String> = row.get("manifest");
    let m: TransferManifest = serde_json::from_str(
        &body.ok_or_else(|| AppError::NotFound("base is not provisioned".into()))?,
    )
    .map_err(internal)?;
    let assigned = binding(&a)? == &m.source && matches_attempt(&m, &a);
    if !assigned || m.role != aq_worker_protocol::SourceRole::Base {
        return Err(AppError::Forbidden("base assignment mismatch".into()));
    }
    Ok(m)
}
pub(super) async fn base_status(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<Uuid>,
) -> Result<Json<TransferManifest>> {
    let n = authenticate_context(&s, &headers).await?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    lock_current_node(&mut tx, &n).await?;
    Ok(Json(load_base(&mut tx, n.id(), id).await?))
}
pub(super) async fn base_download(
    State(s): State<AppState>,
    headers: HeaderMap,
    RoutePath((id, file)): RoutePath<(Uuid, String)>,
    Query(q): Query<DownloadQuery>,
) -> Result<Bytes> {
    let n = authenticate_context(&s, &headers).await?;
    let rt = runtime(&s)?;
    let _bulk = rt.admit_bulk()?;
    if !(1..=chunk_size()).contains(&q.limit) {
        return Err(bad("download chunk limit exceeded"));
    }
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    lock_current_node(&mut tx, &n).await?;
    let m = load_base(&mut tx, n.id(), id).await?;
    let name = filename(&file)?;
    let limit = if name == "manifest.json" {
        m.manifest_bytes
    } else {
        m.bundle_bytes
    };
    if q.offset > limit {
        return Err(bad("download offset outside artifact"));
    }
    let mut f = tokio::fs::File::open(
        rt.config
            .git_root
            .join("artifacts")
            .join(m.artifact_id.to_string())
            .join(name),
    )
    .await
    .map_err(internal)?;
    f.seek(std::io::SeekFrom::Start(q.offset))
        .await
        .map_err(internal)?;
    let mut out = vec![0; q.limit.min((limit - q.offset) as usize)];
    f.read_exact(&mut out).await.map_err(internal)?;
    Ok(Bytes::from(out))
}
