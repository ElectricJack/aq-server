use crate::*;
use sqlx::{PgConnection, PgPool, Row};

pub async fn registry(pool: &PgPool) -> Result<Value> {
    let row =
        sqlx::query("SELECT revision,active_digest FROM profile_catalog_registry WHERE singleton")
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| conflict("profile catalog registry has not been installed"))?;
    Ok(
        json!({"revision":row.get::<i64,_>("revision") as u64,"active_digest":row.get::<Option<String>,_>("active_digest")}),
    )
}

pub async fn store_artifact(pool: &PgPool, digest: &str) -> Result<Value> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("invalid profile catalog artifact"));
    }
    let bytes: Vec<u8> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(digest)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| Error::NotFound("catalog artifact not found".into()))?;
    let catalog = ValidatedCatalog::from_artifact(&bytes).map_err(|_| Error::Internal)?;
    if catalog.digest() != digest {
        return Err(Error::Internal);
    }
    Ok(json!({"digest":digest,"artifact":catalog.artifact()}))
}

async fn catalog(conn: &mut PgConnection) -> Result<(ActivationState, Option<ValidatedCatalog>)> {
    let row = sqlx::query(
        "SELECT revision,active_digest,state FROM profile_catalog_registry WHERE singleton",
    )
    .fetch_one(&mut *conn)
    .await?;
    let state: ActivationState =
        serde_json::from_value(row.get("state")).map_err(|_| Error::Internal)?;
    if state.revision != row.get::<i64, _>("revision") as u64
        || state.active_digest != row.get::<Option<String>, _>("active_digest")
    {
        return Err(Error::Internal);
    }
    let artifact = if let Some(digest) = &state.active_digest {
        let bytes: Vec<u8> =
            sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
                .bind(digest)
                .fetch_one(&mut *conn)
                .await?;
        let validated = ValidatedCatalog::from_artifact(&bytes).map_err(|_| Error::Internal)?;
        if validated.digest() != digest {
            return Err(Error::Internal);
        }
        Some(validated)
    } else {
        None
    };
    Ok((state, artifact))
}
pub async fn read(pool: &PgPool, choices: &[ModelChoice]) -> Result<Value> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let (state, catalog) = catalog(&mut tx).await?;
    let mut profiles = Vec::new();
    if let Some(catalog) = catalog {
        let projects = sqlx::query("SELECT project_id,binding FROM project_execution_bindings WHERE binding->>'catalog_digest'=$1 ORDER BY project_id LIMIT 257")
            .bind(catalog.digest()).fetch_all(&mut *tx).await?;
        if projects.len() > 256 {
            return Err(conflict("Project limit exceeded"));
        }
        for profile in catalog
            .artifact()
            .catalog
            .intelligence
            .iter()
            .filter(|p| p.automatic_routing != Some(true))
        {
            let key = aq_profiles::key(&profile.id, profile.version);
            let mut profile = value(profile)?;
            profile["projects"] = Value::Array(projects.iter().filter_map(|row| {
                let binding: Value = row.get("binding");
                binding["allowed_intelligence_profiles"].as_array()?.iter().any(|p| p.as_str() == Some(&key))
                    .then(|| json!({"id":row.get::<Uuid,_>("project_id"),"name":binding["name"],"enabled":binding["enabled"]}))
            }).collect());
            profiles.push(profile);
        }
    }
    tx.commit().await?;
    Ok(
        json!({"revision":state.revision.to_string(),"active_digest":state.active_digest,"profiles":profiles,"model_choices":choices}),
    )
}
pub async fn operation(pool: &PgPool, actor: Uuid, id: Uuid) -> Result<Value> {
    sqlx::query_scalar(
        "SELECT response FROM profile_catalog_operations WHERE actor_id=$1 AND operation_id=$2",
    )
    .bind(actor)
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| Error::NotFound("Save has not been recorded".into()))
}
pub async fn save(
    pool: &PgPool,
    actor: Uuid,
    req: SaveRequest,
    choices: &[ModelChoice],
    now_ms: i64,
) -> Result<Value> {
    let expected: u64 = req
        .expected_revision
        .parse()
        .map_err(|_| invalid("Invalid profile revision"))?;
    if expected >= i64::MAX as u64 || req.operation_id.is_nil() {
        return Err(invalid("Invalid save identity or revision"));
    }
    let payload_digest =
        aq_profiles::digest(&json!({"kind":"intelligence.settings.save","request":req}))
            .map_err(|_| Error::Internal)?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    // Same order as project_bindings::mutate: writer gate, then registry and
    // project locks. This serializes new bindings and activation atomically.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-project-binding-write-v1',0))")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT revision FROM profile_catalog_registry WHERE singleton FOR UPDATE")
        .fetch_one(&mut *tx)
        .await?;
    if let Some(previous) = sqlx::query("SELECT payload_digest,response FROM profile_catalog_operations WHERE actor_id=$1 AND operation_id=$2")
        .bind(actor).bind(req.operation_id).fetch_optional(&mut *tx).await? {
        if previous.get::<String,_>("payload_digest") != payload_digest { return Err(conflict("Save identity was reused with different changes")); }
        let response = previous.get("response");
        tx.commit().await?;
        return Ok(response);
    }
    let (state, current) = catalog(&mut tx).await?;
    if state.revision != expected {
        return Err(conflict(
            "Profiles changed since this editor opened. Your draft is preserved; reload before saving",
        ));
    }
    let current = current.ok_or_else(|| conflict("No profile catalog is active"))?;
    let project_uses_profile = if req.delete == Some(true) {
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM project_execution_bindings WHERE binding->>'catalog_digest'=$1 AND binding->'allowed_intelligence_profiles' ? $2)")
            .bind(current.digest()).bind(aq_profiles::key(&req.profile_id, req.profile_version)).fetch_one(&mut *tx).await?
    } else {
        false
    };
    let prepared = prepare(&current, &state, &req, choices, project_uses_profile)?;
    let mut updated = Vec::new();
    let mut revision = state.revision;
    let op_count: i64 = sqlx::query_scalar("SELECT count(*) FROM profile_catalog_operations")
        .fetch_one(&mut *tx)
        .await?;
    if op_count >= 100_000 {
        return Err(conflict("Profile save history limit reached"));
    }
    if prepared.changed {
        let next = state
            .prepare_activation(expected, &prepared.catalog)
            .map_err(|_| conflict("Profile identity conflict"))?;
        let next_state = value(&next)?;
        if serde_json::to_vec(&next_state)
            .map_err(|_| Error::Internal)?
            .len()
            > 4 * 1024 * 1024
        {
            return Err(conflict("Profile registry limit reached"));
        }
        revision = next.revision;
        let bytes = prepared
            .catalog
            .canonical_bytes()
            .map_err(|_| Error::Internal)?;
        if bytes.len() > 2_097_152 {
            return Err(invalid("Profile catalog exceeds its size limit"));
        }
        let existing: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
                .bind(prepared.catalog.digest())
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(existing) = existing {
            if existing != bytes {
                return Err(conflict("Profile catalog digest collision"));
            }
        } else {
            let quota = sqlx::query("SELECT count(*) AS count,coalesce(sum(octet_length(artifact)),0)::bigint AS bytes FROM profile_catalog_artifacts").fetch_one(&mut *tx).await?;
            if quota.get::<i64, _>("count") >= 128
                || quota.get::<i64, _>("bytes") + bytes.len() as i64 > 64 * 1024 * 1024
            {
                return Err(conflict("Profile artifact history limit reached"));
            }
            sqlx::query("INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms) VALUES($1,$2,$3,$4)")
            .bind(prepared.catalog.digest()).bind(bytes).bind(actor).bind(now_ms).execute(&mut *tx).await?;
        }
        // Editing intelligence never changes execution environments. Preserve
        // exact activation-time image pins without accepting caller templates.
        sqlx::query("INSERT INTO profile_catalog_environment_templates(catalog_digest,environment,template_digest) SELECT $1,environment,template_digest FROM profile_catalog_environment_templates WHERE catalog_digest=$2 ON CONFLICT (catalog_digest,environment) DO NOTHING")
            .bind(prepared.catalog.digest()).bind(current.digest()).execute(&mut *tx).await?;
        let pins = |digest: &str| {
            sqlx::query_as::<_, (String, String)>("SELECT environment,template_digest FROM profile_catalog_environment_templates WHERE catalog_digest=$1 ORDER BY environment").bind(digest.to_owned())
        };
        let old_pins = pins(current.digest()).fetch_all(&mut *tx).await?;
        let new_pins = pins(prepared.catalog.digest()).fetch_all(&mut *tx).await?;
        if old_pins != new_pins {
            return Err(conflict("Retained catalog environment pins differ"));
        }
        let projects = sqlx::query("SELECT project_id,revision,binding,validated_at_ms,binding_observation FROM project_execution_bindings WHERE binding->>'catalog_digest'=$1 ORDER BY project_id FOR UPDATE")
            .bind(current.digest()).fetch_all(&mut *tx).await?;
        let history: i64 =
            sqlx::query_scalar("SELECT count(*) FROM project_execution_binding_history")
                .fetch_one(&mut *tx)
                .await?;
        if projects.len() > 256 || history + projects.len() as i64 > 100_000 {
            return Err(conflict("Project history limit reached"));
        }
        for project in projects {
            let id: Uuid = project.get("project_id");
            let previous: i64 = project.get("revision");
            let revision = previous
                .checked_add(1)
                .ok_or_else(|| conflict("Project revision limit reached"))?;
            let binding = update_binding(
                &project.get("binding"),
                prepared.catalog.digest(),
                &prepared.references,
            )?;
            let record = json!({"revision":revision,"binding":binding,"validated_at_ms":project.get::<i64,_>("validated_at_ms"),"validation":project.get::<Value,_>("binding_observation")});
            sqlx::query(
                "UPDATE project_execution_bindings SET revision=$2,binding=$3 WHERE project_id=$1",
            )
            .bind(id)
            .bind(revision)
            .bind(&binding)
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO project_execution_binding_history(project_id,revision,record,actor_id,created_at_ms) VALUES($1,$2,$3,$4,$5)")
                .bind(id).bind(revision).bind(record).bind(actor).bind(now_ms).execute(&mut *tx).await?;
            updated.push(json!({"id":id,"name":binding["name"],"revision":revision.to_string()}));
        }
        sqlx::query("UPDATE profile_catalog_registry SET revision=$1,active_digest=$2,state=$3 WHERE singleton")
            .bind(next.revision as i64).bind(prepared.catalog.digest()).bind(next_state).execute(&mut *tx).await?;
    }
    let mut response = json!({"operation_id":req.operation_id,"disposition":if prepared.deleted {"deleted"} else if prepared.changed {"saved"} else {"unchanged"},"revision":revision.to_string(),"active_digest":prepared.catalog.digest(),"profile":prepared.profile,"updated_projects":updated});
    if let Some(routing) = prepared.routing {
        response["routing"] = routing;
    }
    sqlx::query("INSERT INTO profile_catalog_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)")
        .bind(actor).bind(req.operation_id).bind(payload_digest).bind(&response).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(response)
}
