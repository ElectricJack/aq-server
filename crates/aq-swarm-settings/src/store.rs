use crate::*;
use aq_profiles::ValidatedCatalog;
use sqlx::{PgConnection, PgPool, Row};

/// The same singleton lock must cover the scheduler's occupancy check and reservation.
pub async fn lock(conn: &mut PgConnection) -> Result<Settings> {
    settings(conn, true).await
}
async fn settings(conn: &mut PgConnection, locked: bool) -> Result<Settings> {
    let sql = if locked {
        "SELECT revision,active_mode,normal,reduced,limits FROM swarm_settings WHERE singleton FOR UPDATE"
    } else {
        "SELECT revision,active_mode,normal,reduced,limits FROM swarm_settings WHERE singleton"
    };
    let row = sqlx::query(sql).fetch_one(conn).await?;
    let decode = |name| -> Result<Option<Vec<CellLimit>>> {
        row.get::<Option<serde_json::Value>, _>(name)
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| Error::Internal)
    };
    Ok(Settings {
        limits: serde_json::from_value(row.get("limits")).map_err(|_| Error::Internal)?,
        revision: row.get::<i64, _>("revision").to_string(),
        active_mode: match row.get::<String, _>("active_mode").as_str() {
            "normal" => Mode::Normal,
            "reduced" => Mode::Reduced,
            _ => return Err(Error::Internal),
        },
        normal: decode("normal")?,
        reduced: decode("reduced")?,
    })
}
/// Named routing profiles are the authoritative classes; model names never define tiers.
pub async fn catalog(conn: &mut PgConnection) -> Result<Catalog> {
    let row =
        sqlx::query("SELECT revision,active_digest FROM profile_catalog_registry WHERE singleton")
            .fetch_one(&mut *conn)
            .await?;
    let mut cells = std::collections::BTreeMap::new();
    if let Some(digest) = row.get::<Option<String>, _>("active_digest") {
        let bytes: Vec<u8> =
            sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
                .bind(&digest)
                .fetch_one(&mut *conn)
                .await?;
        let catalog = ValidatedCatalog::from_artifact(&bytes).map_err(|_| Error::Internal)?;
        if catalog.digest() != digest {
            return Err(Error::Internal);
        }
        for profile in &catalog.artifact().catalog.intelligence {
            for candidate in &profile.candidates {
                // This is the established non-runnable named-row placeholder.
                if candidate.provider == "configuration-required" {
                    continue;
                }
                cells
                    .entry((candidate.provider.clone(), profile.id.clone()))
                    .or_insert_with(|| CatalogCell {
                        provider: candidate.provider.clone(),
                        intelligence_class: profile.id.clone(),
                        display_name: if profile.automatic_routing == Some(true) {
                            "Automatic routing".into()
                        } else {
                            profile
                                .name
                                .clone()
                                .filter(|n| !n.trim().is_empty())
                                .unwrap_or_else(|| profile.id.clone())
                        },
                    });
            }
        }
    }
    Ok(Catalog {
        revision: row.get::<i64, _>("revision").to_string(),
        cells: cells.into_values().collect(),
    })
}
async fn snapshot(
    conn: &mut PgConnection,
    settings: Settings,
    catalog: Catalog,
) -> Result<Snapshot> {
    let counts = crate::admission::counts(conn).await?;
    let mut unclassified = std::collections::BTreeMap::<String, (u64, u64)>::new();
    for count in &counts {
        if !catalog.cells.iter().any(|c| {
            c.provider == count.provider && c.intelligence_class == count.intelligence_class
        }) {
            let totals = unclassified.entry(count.provider.clone()).or_default();
            totals.0 += u64::from(count.running);
            totals.1 += u64::from(count.waiting);
        }
    }
    let cells = catalog
        .cells
        .into_iter()
        .map(|c| Cell {
            normal_limit: cell_limit(
                settings.normal.as_deref(),
                &c.provider,
                &c.intelligence_class,
            ),
            reduced_limit: cell_limit(
                settings.reduced.as_deref(),
                &c.provider,
                &c.intelligence_class,
            ),
            effective_limit: effective_limit(&settings, &c.provider, &c.intelligence_class),
            running: Some(
                counts
                    .iter()
                    .find(|n| {
                        n.provider == c.provider && n.intelligence_class == c.intelligence_class
                    })
                    .map_or(0, |n| u64::from(n.running)),
            ),
            waiting: Some(
                counts
                    .iter()
                    .find(|n| {
                        n.provider == c.provider && n.intelligence_class == c.intelligence_class
                    })
                    .map_or(0, |n| u64::from(n.waiting)),
            ),
            catalog: c,
        })
        .collect();
    Ok(Snapshot {
        settings,
        catalog_revision: catalog.revision,
        cells,
        unclassified: unclassified
            .into_iter()
            .map(|(provider, (running, waiting))| Unclassified {
                provider,
                running,
                waiting,
            })
            .collect(),
    })
}
pub async fn read(pool: &PgPool) -> Result<Snapshot> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let settings = settings(&mut tx, false).await?;
    let catalog = catalog(&mut tx).await?;
    let snapshot = snapshot(&mut tx, settings, catalog).await?;
    tx.commit().await?;
    Ok(snapshot)
}
pub async fn save(pool: &PgPool, actor: uuid::Uuid, request: SaveRequest) -> Result<Snapshot> {
    let mut tx = pool.begin().await?;
    // Block catalog publication until validation and the complete matrix commit together.
    sqlx::query("SELECT singleton FROM profile_catalog_registry WHERE singleton FOR SHARE")
        .execute(&mut *tx)
        .await?;
    let mut settings = lock(&mut tx).await?;
    let payload = serde_json::json!({"kind":"matrix","request":request});
    if let Some(receipt) = replay(&mut tx, actor, request.operation_id, &payload).await? {
        return Ok(receipt);
    }
    let catalog = catalog(&mut tx).await?;
    if settings.revision != request.expected_revision
        || catalog.revision != request.expected_catalog_revision
    {
        return Err(conflict(
            "Swarm settings or routing catalog changed; reload before saving",
        ));
    }
    validate(&request.normal, &catalog)?;
    if let Some(reduced) = &request.reduced {
        validate(reduced, &catalog)?;
        if reduced.iter().any(|c| {
            Some(c.limit) > cell_limit(Some(&request.normal), &c.provider, &c.intelligence_class)
        }) {
            return Err(invalid("Reduced usage limits cannot exceed Normal limits"));
        }
    }
    if settings.active_mode == Mode::Reduced && request.reduced.is_none() {
        return Err(invalid(
            "Switch to Normal before removing the Reduced usage profile",
        ));
    }
    settings.normal = Some(request.normal);
    settings.reduced = request.reduced;
    persist(&mut tx, &mut settings).await?;
    let snapshot = snapshot(&mut tx, settings, catalog).await?;
    record(&mut tx, actor, request.operation_id, payload, &snapshot).await?;
    tx.commit().await?;
    Ok(snapshot)
}
/// Patch only the selected provider or class, preserving every unrelated limit.
pub async fn set_limits(
    pool: &PgPool,
    actor: uuid::Uuid,
    request: LimitsRequest,
) -> Result<Snapshot> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT singleton FROM profile_catalog_registry WHERE singleton FOR SHARE")
        .execute(&mut *tx)
        .await?;
    let mut settings = lock(&mut tx).await?;
    let payload = serde_json::json!({"kind":"limits","request":request});
    if let Some(receipt) = replay(&mut tx, actor, request.operation_id, &payload).await? {
        return Ok(receipt);
    }
    let catalog = catalog(&mut tx).await?;
    if settings.revision != request.expected_revision
        || catalog.revision != request.expected_catalog_revision
    {
        return Err(conflict(
            "Swarm settings or routing catalog changed; reload before saving",
        ));
    }
    if request.provider.is_none() && request.cell.is_none() {
        return Err(invalid("Select a provider or intelligence class limit"));
    }
    if let Some(provider) = request.provider {
        if !catalog
            .cells
            .iter()
            .any(|c| c.provider == provider.provider)
        {
            return Err(invalid("Select a supported provider"));
        }
        settings
            .limits
            .provider_limits
            .retain(|p| p.provider != provider.provider);
        settings.limits.provider_limits.push(provider);
        settings
            .limits
            .provider_limits
            .sort_by(|a, b| a.provider.cmp(&b.provider));
    }
    if let Some(cell) = request.cell {
        if !catalog
            .cells
            .iter()
            .any(|c| c.provider == cell.provider && c.intelligence_class == cell.intelligence_class)
        {
            return Err(invalid(
                "Select a supported provider and intelligence class",
            ));
        }
        settings.limits.cell_limits.retain(|c| {
            c.provider != cell.provider || c.intelligence_class != cell.intelligence_class
        });
        settings.limits.cell_limits.push(cell);
        settings.limits.cell_limits.sort_by(|a, b| {
            (&a.provider, &a.intelligence_class).cmp(&(&b.provider, &b.intelligence_class))
        });
    }
    persist(&mut tx, &mut settings).await?;
    let snapshot = snapshot(&mut tx, settings, catalog).await?;
    record(&mut tx, actor, request.operation_id, payload, &snapshot).await?;
    tx.commit().await?;
    Ok(snapshot)
}
pub async fn set_mode(pool: &PgPool, actor: uuid::Uuid, request: ModeRequest) -> Result<Snapshot> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT singleton FROM profile_catalog_registry WHERE singleton FOR SHARE")
        .execute(&mut *tx)
        .await?;
    let mut settings = lock(&mut tx).await?;
    let payload = serde_json::json!({"kind":"mode","request":request});
    if let Some(receipt) = replay(&mut tx, actor, request.operation_id, &payload).await? {
        return Ok(receipt);
    }
    if settings.revision != request.expected_revision {
        return Err(conflict(
            "Swarm settings changed; reload before switching modes",
        ));
    }
    let catalog = catalog(&mut tx).await?;
    if request.active_mode == Mode::Reduced {
        let reduced = settings
            .reduced
            .as_deref()
            .ok_or_else(|| invalid("Configure Reduced usage before enabling it"))?;
        validate(reduced, &catalog)?;
    }
    settings.active_mode = request.active_mode;
    persist(&mut tx, &mut settings).await?;
    let snapshot = snapshot(&mut tx, settings, catalog).await?;
    record(&mut tx, actor, request.operation_id, payload, &snapshot).await?;
    tx.commit().await?;
    Ok(snapshot)
}
async fn persist(conn: &mut PgConnection, settings: &mut Settings) -> Result<()> {
    let mode = match settings.active_mode {
        Mode::Normal => "normal",
        Mode::Reduced => "reduced",
    };
    let normal = settings
        .normal
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| Error::Internal)?;
    let reduced = settings
        .reduced
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| Error::Internal)?;
    let revision: i64 = sqlx::query_scalar("UPDATE swarm_settings SET revision=revision+1,active_mode=$1,normal=$2,reduced=$3,limits=$4 WHERE singleton RETURNING revision")
        .bind(mode).bind(normal).bind(reduced).bind(serde_json::to_value(&settings.limits).map_err(|_| Error::Internal)?).fetch_one(conn).await?;
    settings.revision = revision.to_string();
    Ok(())
}

async fn replay(
    conn: &mut PgConnection,
    actor: uuid::Uuid,
    id: uuid::Uuid,
    request: &serde_json::Value,
) -> Result<Option<Snapshot>> {
    if id.is_nil() {
        return Err(invalid("Enter a valid operation ID"));
    }
    let row = sqlx::query(
        "SELECT actor_id,request,receipt FROM swarm_setting_operations WHERE operation_id=$1",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.get::<uuid::Uuid, _>("actor_id") != actor
        || row.get::<serde_json::Value, _>("request") != *request
    {
        return Err(conflict("Operation ID already used for another request"));
    }
    serde_json::from_value(row.get("receipt"))
        .map(Some)
        .map_err(|_| Error::Internal)
}
async fn record(
    conn: &mut PgConnection,
    actor: uuid::Uuid,
    id: uuid::Uuid,
    request: serde_json::Value,
    receipt: &Snapshot,
) -> Result<()> {
    let receipt = serde_json::to_value(receipt).map_err(|_| Error::Internal)?;
    sqlx::query("INSERT INTO swarm_setting_operations (operation_id,actor_id,request,receipt) VALUES ($1,$2,$3,$4)")
        .bind(id).bind(actor).bind(request).bind(receipt).execute(conn).await?;
    Ok(())
}
pub async fn operation(pool: &PgPool, actor: uuid::Uuid, id: uuid::Uuid) -> Result<Snapshot> {
    let value: serde_json::Value = sqlx::query_scalar(
        "SELECT receipt FROM swarm_setting_operations WHERE operation_id=$1 AND actor_id=$2",
    )
    .bind(id)
    .bind(actor)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| Error::NotFound("Save has not been recorded".into()))?;
    serde_json::from_value(value).map_err(|_| Error::Internal)
}
