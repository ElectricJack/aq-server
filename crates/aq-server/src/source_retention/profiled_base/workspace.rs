//! Standalone bundles for approved sibling base commits, using the central
//! artifact store's size limits and stable, repository-scoped export identity.
use super::*;
use aq_worker_protocol::{ObjectAlgorithm, ObjectId};

const CHUNK: u64 = 1_048_576;

pub(crate) async fn chunk(
    state: &AppState,
    project: Uuid,
    repository: Uuid,
    generation: u64,
    base: &str,
    offset: u64,
) -> Result<(u64, String, Vec<u8>)> {
    if !offset.is_multiple_of(CHUNK) {
        return Err(refused());
    }
    let rt = runtime(state)?;
    let _bulk = rt.admit_bulk()?;
    let identity = format!("{repository}/{generation}/{base}");
    let operation = Uuid::new_v5(&Uuid::from_bytes(*b"aq-workspace-pin"), identity.as_bytes());
    let object = ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: base.into(),
    };
    let root = rt.config.git_root.clone();
    let max = rt.config.max_bundle_bytes;
    let mut out = read_chunk(
        root.clone(),
        repository,
        generation,
        object.clone(),
        operation,
        max,
        offset,
    )
    .await?;
    if out.is_none() && offset == 0 {
        let source_handle = recover_handle(state, project, repository, generation, base).await?;
        reserve(state, repository, generation, base, max).await?;
        let request = json!({
            "protocol_version":1, "request_id":operation, "operation":"artifact.export",
            "repository_id":repository, "operation_id":operation,
            "expected_binding_generation":generation,
            "params":{
                "source":object, "base":object, "retention_handle":source_handle,
                "confirmed_prerequisites":[], "reference_only":false,
                "caller_correlation":{"owner":"aq.workspace-base-v1"}
            },
            "deadline_ms":30000
        });
        let response = exchange(&rt, &request, MAX_MANIFEST, Duration::from_secs(35)).await?;
        let manifest: GitManifest =
            serde_json::from_value(response["manifest"].clone()).map_err(|_| refused())?;
        if !manifest
            .matches_export_response(&response, operation)
            .map_err(|_| refused())?
        {
            return Err(refused());
        }
        out = read_chunk(root, repository, generation, object, operation, max, offset).await?;
    }
    let (size, digest, bytes) = out.ok_or_else(refused)?;
    if offset == 0 {
        let repo = aq_delivery::source_checks::Repository {
            id: repository,
            generation,
        };
        let actual = aq_delivery::offer_source::read(
            &rt.config.git_socket,
            repo,
            "retention.inspect",
            json!({"handle_id":operation,"limit":1}),
        )
        .await
        .map_err(|_| refused())?;
        if !aq_delivery::source_checks::retention_holds(&actual, operation, repo, base) {
            return Err(refused());
        }
        sqlx::query(
            "UPDATE task_workspace_retained_bases SET reserved_bytes=$4
             WHERE repository_id=$1 AND binding_generation=$2 AND commit_oid=$3
               AND reserved_bytes > $4",
        )
        .bind(repository)
        .bind(generation as i64)
        .bind(base)
        .bind(size as i64)
        .execute(&state.store.pool)
        .await
        .map_err(crate::error::db)?;
    }
    Ok((size, digest, bytes))
}

async fn recover_handle(
    state: &AppState,
    project: Uuid,
    repository: Uuid,
    generation: u64,
    base: &str,
) -> Result<Uuid> {
    let pool = &state.store.pool;
    let recorded: Option<Uuid> = sqlx::query_scalar(
        "SELECT source_handle FROM task_workspace_retained_bases
         WHERE repository_id=$1 AND binding_generation=$2 AND commit_oid=$3",
    )
    .bind(repository)
    .bind(generation as i64)
    .bind(base)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::db)?;
    if let Some(handle) = recorded {
        return Ok(handle);
    }

    // Rows written before this migration carry the exact source but no handle.
    // Recover only a trusted historical handle or the current policy's handle
    // if it still owns that object, without resolving a moving ref.
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(refused)?;
    let policy = config
        .project_policy_record(project)
        .map_err(|_| refused())?;
    let historic: Option<Uuid> = sqlx::query_scalar(
        "SELECT retention_handle FROM profiled_main_syncs WHERE project_id=$1
         AND repository_id=$2 AND binding_generation=$3 AND head=$4
         ORDER BY synced_at_ms DESC LIMIT 1",
    )
    .bind(project)
    .bind(repository)
    .bind(generation as i64)
    .bind(base)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::db)?;
    let candidate = historic.unwrap_or(policy.retention_handle);
    let repo = aq_delivery::source_checks::Repository {
        id: repository,
        generation,
    };
    let rt = runtime(state)?;
    let value = aq_delivery::offer_source::read(
        &rt.config.git_socket,
        repo,
        "retention.inspect",
        json!({"handle_id":candidate,"limit":1}),
    )
    .await
    .map_err(|_| refused())?;
    if !aq_delivery::source_checks::retention_holds(&value, candidate, repo, base) {
        return Err(refused());
    }
    sqlx::query(
        "INSERT INTO task_workspace_retained_bases(repository_id,binding_generation,commit_oid,source_handle)
         VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
    )
    .bind(repository)
    .bind(generation as i64)
    .bind(base)
    .bind(candidate)
    .execute(pool)
    .await
    .map_err(crate::error::db)?;
    // Concurrent legacy retries share one source handle and export operation.
    sqlx::query_scalar(
        "SELECT source_handle FROM task_workspace_retained_bases
         WHERE repository_id=$1 AND binding_generation=$2 AND commit_oid=$3",
    )
    .bind(repository)
    .bind(generation as i64)
    .bind(base)
    .fetch_one(pool)
    .await
    .map_err(crate::error::db)
}

async fn reserve(
    state: &AppState,
    repository: Uuid,
    generation: u64,
    base: &str,
    limit: u64,
) -> Result<()> {
    let quota = runtime(state)?.config.max_staging_bytes;
    let mut tx = state.store.pool.begin().await.map_err(crate::error::db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('workspace-bundle-quota',0))")
        .execute(&mut *tx)
        .await
        .map_err(crate::error::db)?;
    let reserved: Option<i64> = sqlx::query_scalar(
        "SELECT reserved_bytes FROM task_workspace_retained_bases
         WHERE repository_id=$1 AND binding_generation=$2 AND commit_oid=$3",
    )
    .bind(repository)
    .bind(generation as i64)
    .bind(base)
    .fetch_optional(&mut *tx)
    .await
    .map_err(crate::error::db)?;
    if reserved == Some(0) {
        let fits: bool = sqlx::query_scalar(
            "SELECT COALESCE(sum(reserved_bytes),0)+$1::bigint <= $2::bigint FROM task_workspace_retained_bases",
        )
        .bind(limit as i64)
        .bind(quota as i64)
        .fetch_one(&mut *tx)
        .await
        .map_err(crate::error::db)?;
        if !fits {
            return Err(pending());
        }
        sqlx::query(
            "UPDATE task_workspace_retained_bases SET reserved_bytes=$4
             WHERE repository_id=$1 AND binding_generation=$2 AND commit_oid=$3",
        )
        .bind(repository)
        .bind(generation as i64)
        .bind(base)
        .bind(limit as i64)
        .execute(&mut *tx)
        .await
        .map_err(crate::error::db)?;
    } else if reserved.is_none() {
        return Err(refused());
    }
    tx.commit().await.map_err(crate::error::db)?;
    Ok(())
}

async fn read_chunk(
    root: std::path::PathBuf,
    repository: Uuid,
    generation: u64,
    object: ObjectId,
    operation: Uuid,
    max: u64,
    offset: u64,
) -> Result<Option<(u64, String, Vec<u8>)>> {
    let slot = file_slot()?;
    let work = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        let root = Installation::open(&root).map_err(|_| refused())?;
        let Ok(directory) = root.artifact(operation) else {
            return Ok(None);
        };
        let Ok(manifest) = directory.manifest(MAX_MANIFEST) else {
            return Ok(None);
        };
        let manifest: GitManifest = serde_json::from_slice(&manifest).map_err(|_| refused())?;
        if manifest.transfer_id != operation.to_string()
            || manifest.repository_id != repository.to_string()
            || manifest.binding_generation != generation
            || manifest.producer_installation_id != root.id().map_err(|_| refused())?.to_string()
            || !manifest.is_full_base(&object, max)
            || manifest.caller_correlation != json!({"owner":"aq.workspace-base-v1"})
        {
            return Err(refused());
        }
        let digest = manifest
            .bundle_sha256
            .filter(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            .ok_or_else(refused)?;
        let remaining = manifest
            .bundle_size
            .checked_sub(offset)
            .filter(|n| *n > 0)
            .ok_or_else(refused)?;
        let bytes = directory
            .read_bundle(manifest.bundle_size, offset, remaining.min(CHUNK) as usize)
            .map_err(|_| refused())?;
        Ok(Some((manifest.bundle_size, digest, bytes)))
    });
    tokio::time::timeout(Duration::from_secs(5), work)
        .await
        .map_err(|_| pending())?
        .map_err(|_| pending())?
}
