use super::*;

async fn hash_file(path: &Path) -> Result<String> {
    aq_delivery::artifact_files::digest(path)
        .await
        .map(|(hash, _)| hash)
        .map_err(internal)
}
async fn socket_import(rt: &Runtime, t: &Transfer) -> Result<Value> {
    let request_id = format!("source-import-{}", t.op);
    let request = json!({"protocol_version":1,"request_id":request_id,"operation":"artifact.import","repository_id":t.manifest.source.repository_id,"operation_id":t.op,"expected_binding_generation":t.manifest.source.binding_generation,"params":{"artifact_handle":t.manifest.artifact_id},"deadline_ms":rt.config.git_deadline_ms});
    let exchange = artifact_exchange(rt, &request);
    tokio::time::timeout(rt.git_wait(), exchange)
        .await
        .map_err(|_| AppError::UpdatesUnavailable("import pending; retry same transfer".into()))?
        .map_err(|e| {
            tracing::warn!(error=%e,operation_id=%t.op,"import outcome remains pending");
            AppError::UpdatesUnavailable("import pending; retry same transfer".into())
        })
}
pub(super) async fn finalize(
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
    let rt = runtime(&s)?;
    let _bulk = rt.admit_bulk()?;
    let _gate = rt.finalize_gate.lock().await;
    let (attempt, mut transfer) = load_for_import(&s, &n, r.transfer_id).await?;
    let import = Import {
        state: &s,
        node: &n,
        runtime: &rt,
        attempt,
        id: r.transfer_id,
    };
    let status = if transfer.receipt.is_some() {
        response(import.id, &transfer)
    } else {
        import.run(&mut transfer).await?
    };
    crate::delegated_permits::receipts::source(&s, &n, status).await
}

async fn load_for_import(
    s: &AppState,
    n: &AuthenticatedNode,
    id: Uuid,
) -> Result<(Uuid, Transfer)> {
    let mut tx = s.store.pool.begin().await.map_err(internal)?;
    let attempt = transfer_attempt(&mut tx, n.id(), id).await?;
    let mut t = authorized_transfer(&mut tx, n, attempt, id).await?;
    if t.state == "rejected" {
        return Err(conflict("source upload expired"));
    }
    if t.receipt.is_none() {
        t.prepare(&mut tx, id).await?;
    }
    tx.commit().await.map_err(internal)?;
    Ok((attempt, t))
}

impl Transfer {
    async fn prepare(&mut self, tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<()> {
        if self.manifest_offset != self.manifest.manifest_bytes
            || self.bundle_offset != self.manifest.bundle_bytes
        {
            return Err(conflict("source upload incomplete"));
        }
        // Protect complete bytes from expiry cleanup before releasing locks for
        // validation. The durable operation identity already exists; no Git call
        // occurs until all transport and frozen-manifest checks pass.
        if self.state == "uploading" {
            sqlx::query("UPDATE source_transfers SET state='import_pending',updated_at=now() WHERE transfer_id=$1 AND state='uploading'").bind(id).execute(&mut **tx).await.map_err(internal)?;
            self.state = "import_pending".into();
        }
        Ok(())
    }
}

struct Import<'a> {
    state: &'a AppState,
    node: &'a AuthenticatedNode,
    runtime: &'a Runtime,
    attempt: Uuid,
    id: Uuid,
}

enum Dispatch {
    Retained(TransferStatus),
    Admitted(Validated),
}

struct Validated {
    manifest: GitManifest,
    digest: String,
}

impl Import<'_> {
    async fn run(&self, transfer: &mut Transfer) -> Result<TransferStatus> {
        let paths = TransportPaths::new(self.runtime, self.id, transfer.manifest.artifact_id);
        let validation = paths.validate(transfer).await;
        let validated = match self
            .admit(transfer.manifest.artifact_id, validation)
            .await?
        {
            Dispatch::Retained(status) => return Ok(status),
            Dispatch::Admitted(validated) => validated,
        };
        paths.publish().await?;
        let imported = socket_import(self.runtime, transfer).await;
        self.confirm(transfer, validated, imported).await?;
        self.release_staging(&paths).await?;
        Ok(response(self.id, transfer))
    }
    async fn admit(&self, artifact: Uuid, validated: Result<Validated>) -> Result<Dispatch> {
        // Recheck the same HTTP principal after filesystem waits before either
        // rejecting transport or admitting its physical import. The UUID-only
        // trusted owner loader remains reusable by internal readers.
        let mut import_tx = self.state.store.pool.begin().await.map_err(internal)?;
        let latest = authorized_transfer(&mut import_tx, self.node, self.attempt, self.id).await?;
        if latest.receipt.is_some() {
            import_tx.commit().await.map_err(internal)?;
            return Ok(Dispatch::Retained(response(self.id, &latest)));
        }
        if latest.state == "rejected" {
            return Err(conflict("source transfer rejected"));
        }
        let validated = match validated {
            Ok(v) => v,
            Err(e) => {
                if matches!(
                    e,
                    AppError::BadRequest(_) | AppError::Forbidden(_) | AppError::Conflict(_)
                ) {
                    self.reject(import_tx, artifact).await?;
                }
                return Err(e);
            }
        };
        sqlx::query("UPDATE source_transfers SET dispatch_intent=TRUE WHERE transfer_id=$1")
            .bind(self.id)
            .execute(&mut *import_tx)
            .await
            .map_err(internal)?;
        import_tx.commit().await.map_err(internal)?;
        Ok(Dispatch::Admitted(validated))
    }
    async fn reject(&self, mut import_tx: Transaction<'_, Postgres>, artifact: Uuid) -> Result<()> {
        // Never discard bytes for an uncertain already-dispatched import.
        let changed=sqlx::query("UPDATE source_transfers SET state='rejected' WHERE transfer_id=$1 AND state='import_pending' AND NOT dispatch_intent AND receipt IS NULL").bind(self.id).execute(&mut *import_tx).await.map_err(internal)?.rows_affected();
        import_tx.commit().await.map_err(internal)?;
        if changed == 1 {
            discard_transport(self.runtime, self.id, artifact).await?;
            sqlx::query("UPDATE source_transfers SET staging_released=TRUE WHERE transfer_id=$1 AND state='rejected' AND NOT dispatch_intent").bind(self.id).execute(&self.state.store.pool).await.map_err(internal)?;
        }
        Ok(())
    }
    async fn confirm(
        &self,
        t: &mut Transfer,
        validated: Validated,
        imported: Result<Value>,
    ) -> Result<()> {
        // A failed/late physical reply cannot bypass the original principal fence.
        // Already admitted import intent/bytes stay protected for a fresh caller.
        let mut tx = self.state.store.pool.begin().await.map_err(internal)?;
        machine_authority(&mut tx, self.node, self.attempt).await?;
        transfer(&mut tx, self.node.id(), self.id).await?;
        let receipt = imported?;
        let b = &t.manifest.source;
        let Validated {
            manifest: m,
            digest: canonical_digest,
        } = validated;
        let projection = CentralRetentionReceipt {
            receipt_id: t.op,
            transfer_id: self.id,
            repository_id: b.repository_id,
            binding_generation: b.binding_generation,
            source: m.source,
            retention_handle: t.manifest.artifact_id,
            manifest_digest: canonical_digest,
        };
        if !aq_delivery::source_transport::matches_import_receipt(&receipt, &projection)
            .map_err(internal)?
        {
            return Err(AppError::UpdatesUnavailable(
                "unverified import receipt; transfer remains pending".into(),
            ));
        }
        t.receipt = Some(projection);
        sqlx::query("UPDATE source_transfers SET state='retained',receipt=$2::jsonb,updated_at=now() WHERE transfer_id=$1").bind(self.id).bind(serde_json::to_string(&t.receipt).map_err(internal)?).execute(&mut *tx).await.map_err(internal)?;
        // Preserve complete raw central receipt as audit evidence, not just its projection.
        sqlx::query("UPDATE source_transfers SET central_receipt=$2::jsonb WHERE transfer_id=$1")
            .bind(self.id)
            .bind(serde_json::to_string(&receipt).map_err(internal)?)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(())
    }
    async fn release_staging(&self, paths: &TransportPaths) -> Result<()> {
        // Central retained objects own durability now; incoming transport bytes are expendable.
        match tokio::fs::remove_dir_all(&paths.incoming).await {
            Ok(()) => {
                sqlx::query("UPDATE source_transfers SET staging_released=TRUE WHERE transfer_id=$1 AND receipt IS NOT NULL").bind(self.id).execute(&self.state.store.pool).await.map_err(internal)?;
            }
            Err(e) => tracing::warn!(error=%e,"retained incoming staging cleanup deferred"),
        }
        Ok(())
    }
}

struct TransportPaths {
    incoming: PathBuf,
    staging: PathBuf,
}
impl TransportPaths {
    fn new(rt: &Runtime, id: Uuid, artifact: Uuid) -> Self {
        Self {
            incoming: rt
                .config
                .git_root
                .join("artifacts/incoming")
                .join(artifact.to_string()),
            staging: rt.path(id),
        }
    }
    async fn validate(&self, t: &Transfer) -> Result<Validated> {
        let published = tokio::fs::try_exists(&self.incoming)
            .await
            .map_err(internal)?;
        let path = if published {
            &self.incoming
        } else {
            &self.staging
        };
        if hash_file(&path.join("manifest.json")).await? != t.manifest.manifest_digest {
            return Err(conflict("manifest transport digest mismatch"));
        }
        if let Some(expected) = &t.manifest.bundle_digest
            && hash_file(&path.join("objects.bundle")).await? != *expected
        {
            return Err(conflict("bundle transport digest mismatch"));
        }
        let bytes = tokio::fs::read(path.join("manifest.json"))
            .await
            .map_err(internal)?;
        let m: GitManifest =
            serde_json::from_slice(&bytes).map_err(|_| bad("invalid aq-git manifest"))?;
        verify_transport(&m, &t.manifest)?;
        let digest = digest(&serde_json::to_vec(&m).map_err(internal)?);
        Ok(Validated {
            manifest: m,
            digest,
        })
    }
    async fn publish(&self) -> Result<()> {
        if !tokio::fs::try_exists(&self.incoming)
            .await
            .map_err(internal)?
        {
            // Atomic rename only: deployments must place private staging on git_root's filesystem.

            tokio::fs::create_dir_all(self.incoming.parent().unwrap())
                .await
                .map_err(internal)?;
            tokio::fs::rename(&self.staging, &self.incoming)
                .await
                .map_err(internal)?;
            let dir = tokio::fs::File::open(self.incoming.parent().unwrap())
                .await
                .map_err(internal)?;
            dir.sync_all().await.map_err(internal)?;
        }
        Ok(())
    }
}

fn verify_transport(git: &GitManifest, manifest: &TransferManifest) -> Result<()> {
    if !git.matches_transfer(manifest) {
        return Err(AppError::Forbidden(
            "artifact does not match frozen source binding".into(),
        ));
    }
    if git.source != manifest.checkpoint
        || manifest.role != aq_worker_protocol::SourceRole::Checkpoint
    {
        return Err(AppError::Forbidden(
            "artifact role or checkpoint mismatch".into(),
        ));
    }
    git.source.validate().map_err(|_| bad("invalid source oid"))
}
