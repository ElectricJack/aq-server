use super::*;

pub(super) async fn begin(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(r): ApiJson<BeginRequest>,
) -> Result<Json<TransferStatus>> {
    r.identity
        .validate()
        .map_err(|_| bad("invalid node identity"))?;
    let n = authenticate_upload(&s, &headers).await?;
    if r.identity.node_id != n.id() {
        return Err(AppError::Forbidden("node identity mismatch".into()));
    }
    let rt = runtime(&s)?;
    let _bulk = rt.admit_bulk()?;
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let a = machine_authority(&mut tx, &n, r.manifest.attempt.attempt_id).await?;
    check_manifest(&a, &r.manifest)?;
    let operation = UploadOperation {
        node: n.id(),
        id: r.operation_id,
        digest: digest(&serde_json::to_vec(&r).map_err(internal)?),
    };
    if let Some(old) = operation.replay(&mut tx).await? {
        return Ok(Json(old));
    }
    r.check_new_transfer(&mut tx, &rt, a.retention_released)
        .await?;
    r.check_staging(&mut tx, &rt, &s).await?;
    let out = r.reserve(&mut tx, n.id()).await?;
    operation.record(&mut tx, &out).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(out))
}
pub(super) async fn status(
    State(s): State<AppState>,
    headers: HeaderMap,
    ApiJson(r): ApiJson<TransferStatusRequest>,
) -> Result<axum::response::Response> {
    r.validate()
        .map_err(|_| bad("invalid source status request"))?;
    let n = authenticate_upload(&s, &headers).await?;
    if n.id() != r.identity.node_id {
        return Err(AppError::Forbidden("node identity mismatch".into()));
    }
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    // Discover attempt before taking row lock so lock ordering stays Node->Attempt->Transfer.
    let attempt = transfer_attempt(&mut tx, n.id(), r.transfer_id).await?;
    let t = authorized_transfer(&mut tx, &n, attempt, r.transfer_id).await?;
    tx.commit().await.map_err(internal)?;
    crate::delegated_permits::receipts::source(&s, &n, response(r.transfer_id, &t)).await
}
type ChunkLocation = (RoutePath<(Uuid, String)>, Query<ChunkQuery>);
pub(super) async fn chunk(
    State(s): State<AppState>,
    headers: HeaderMap,
    (RoutePath((id, file)), Query(q)): ChunkLocation,
    bytes: Bytes,
) -> Result<Json<TransferStatus>> {
    let n = authenticate_upload(&s, &headers).await?;
    let rt = runtime(&s)?;
    let _bulk = rt.admit_bulk()?;
    let name = filename(&file)?;
    if !(1..=chunk_size()).contains(&bytes.len()) || q.digest != digest(&bytes) {
        return Err(bad("invalid chunk size or digest"));
    }
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let attempt = transfer_attempt(&mut tx, n.id(), id).await?;
    let mut t = authorized_transfer(&mut tx, &n, attempt, id).await?;
    let operation = UploadOperation {
        node: n.id(),
        id: q.operation_id,
        digest: digest(format!("{id}/{file}/{}/{}/{}", q.offset, bytes.len(), q.digest).as_bytes()),
    };
    if let Some(old) = operation.replay(&mut tx).await? {
        return Ok(Json(old));
    }
    let offset = t.chunk_offset(name, q.offset, bytes.len() as u64)?;
    append_chunk(&rt.path(id), name, offset, &bytes).await?;
    let next = offset + bytes.len() as u64;
    if name == "manifest.json" {
        t.manifest_offset = next;
    } else {
        t.bundle_offset = next;
    }
    sqlx::query("UPDATE source_transfers SET manifest_offset=$2,bundle_offset=$3,updated_at=now() WHERE transfer_id=$1").bind(id).bind(t.manifest_offset as i64).bind(t.bundle_offset as i64).execute(&mut *tx).await.map_err(internal)?;
    let out = response(id, &t);
    operation.record(&mut tx, &out).await?;
    tx.commit().await.map_err(internal)?;
    Ok(Json(out))
}

struct UploadOperation {
    node: Uuid,
    id: Uuid,
    digest: String,
}
impl UploadOperation {
    async fn replay(&self, tx: &mut Transaction<'_, Postgres>) -> Result<Option<TransferStatus>> {
        if self.id.is_nil() {
            return Err(bad("operation id required"));
        }
        let row=sqlx::query("SELECT payload_digest,response::text AS response FROM source_transfer_operations WHERE node_id=$1 AND operation_id=$2").bind(self.node).bind(self.id).fetch_optional(&mut **tx).await.map_err(internal)?;
        if let Some(r) = row {
            if r.get::<&str, _>("payload_digest") != self.digest {
                return Err(conflict("operation id payload conflict"));
            }
            return Ok(Some(
                serde_json::from_str(r.get("response")).map_err(internal)?,
            ));
        }
        Ok(None)
    }
    async fn record(&self, tx: &mut Transaction<'_, Postgres>, r: &TransferStatus) -> Result<()> {
        sqlx::query("INSERT INTO source_transfer_operations(node_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4::jsonb)").bind(self.node).bind(self.id).bind(&self.digest).bind(serde_json::to_string(r).map_err(internal)?).execute(&mut **tx).await.map_err(internal)?;
        Ok(())
    }
}

impl BeginRequest {
    async fn check_new_transfer(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        rt: &Runtime,
        released: bool,
    ) -> Result<()> {
        if crate::profiled_execution::has_delivery_grant(
            &mut **tx,
            self.manifest.attempt.attempt_id,
        )
        .await?
        {
            return Err(AppError::Forbidden(
                "attempt retains source through its Git grant".into(),
            ));
        }
        // Historical retries retain their response, but a released attempt cannot
        // acquire a new source-transfer intent or task claim.
        if released {
            return Err(conflict("attempt retention has already been released"));
        }
        if self.manifest.bundle_bytes > rt.config.max_bundle_bytes {
            return Err(AppError::PayloadTooLarge("bundle limit exceeded".into()));
        }
        Ok(())
    }
    async fn check_staging(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        rt: &Runtime,
        s: &AppState,
    ) -> Result<()> {
        // Serialize reservations across nodes; accounting includes uncertain imports.
        sqlx::query("SELECT pg_advisory_xact_lock(719247031)")
            .execute(&mut **tx)
            .await
            .map_err(internal)?;
        cleanup(rt, tx).await?;
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM source_transfers WHERE NOT staging_released")
                .fetch_one(&mut **tx)
                .await
                .map_err(internal)?;
        if count >= 1024 {
            return Err(AppError::PayloadTooLarge(
                "source transfer count limit exceeded".into(),
            ));
        }
        let used = aq_history_caps::staging_used(tx, (s.clock)().timestamp_millis())
            .await
            .map_err(internal)?;
        if (used as u64)
            .saturating_add(self.manifest.manifest_bytes)
            .saturating_add(self.manifest.bundle_bytes)
            > rt.config.max_staging_bytes
        {
            return Err(AppError::PayloadTooLarge(
                "source staging quota exhausted".into(),
            ));
        }
        Ok(())
    }
    async fn reserve(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        node: Uuid,
    ) -> Result<TransferStatus> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM source_transfers WHERE transfer_id=$1 OR artifact_id=$2)",
        )
        .bind(self.manifest.transfer_id)
        .bind(self.manifest.artifact_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(internal)?;
        if exists {
            return Err(conflict("transfer or artifact identity already exists"));
        }
        // Reserve durable ownership before creating files. Chunk writes lazily create
        // private members, so cancellation cannot leave an unaccounted orphan.

        sqlx::query("INSERT INTO source_transfers(transfer_id,attempt_id,node_id,repository_id,import_operation_id,manifest,artifact_id) VALUES($1,$2,$3,$4,$5,$6::jsonb,$7)").bind(self.manifest.transfer_id).bind(self.manifest.attempt.attempt_id).bind(node).bind(self.manifest.source.repository_id).bind(Uuid::new_v4()).bind(serde_json::to_string(&self.manifest).map_err(internal)?).bind(self.manifest.artifact_id).execute(&mut **tx).await.map_err(internal)?;
        Ok(TransferStatus {
            transfer_id: self.manifest.transfer_id,
            manifest_offset: 0,
            bundle_offset: 0,
            receipt: None,
        })
    }
}

impl Transfer {
    fn chunk_offset(&self, name: &str, requested: u64, bytes: u64) -> Result<u64> {
        if self.state != "uploading" {
            return Err(conflict("transfer no longer accepts chunks"));
        }
        let (offset, limit) = if name == "manifest.json" {
            (self.manifest_offset, self.manifest.manifest_bytes)
        } else {
            (self.bundle_offset, self.manifest.bundle_bytes)
        };
        if requested != offset || offset.saturating_add(bytes) > limit {
            return Err(conflict("chunk offset or declared length mismatch"));
        }
        Ok(offset)
    }
}
async fn append_chunk(directory: &Path, name: &str, offset: u64, bytes: &[u8]) -> Result<()> {
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(internal)?;
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(name))
        .await
        .map_err(internal)?;
    // Discard any unacknowledged bytes from a crash before the SQL commit.
    f.set_len(offset).await.map_err(internal)?;
    f.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(internal)?;
    f.write_all(bytes).await.map_err(internal)?;
    f.sync_all().await.map_err(internal)?;
    Ok(())
}
