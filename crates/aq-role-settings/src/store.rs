use crate::*;
use aq_profiles::roles::{self, SourceDocument};
use sqlx::{PgConnection, PgPool, Row};

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Role => "role",
        Kind::Intelligence => "intelligence",
        Kind::Execution => "execution",
    }
}
fn kind_of(name: &str) -> Result<Kind> {
    match name {
        "role" => Ok(Kind::Role),
        "intelligence" => Ok(Kind::Intelligence),
        "execution" => Ok(Kind::Execution),
        _ => Err(Error::Internal),
    }
}
/// A stored snapshot is read as stored (spec 0014 §7.1): its bytes must hash
/// to the digest they are stored under and deserialize. Document validation
/// belongs to compiling the current set, not to reading an old one.
fn stored_snapshot(bytes: &[u8], digest: &str) -> Result<Snapshot> {
    let snapshot = Snapshot::from_stored_bytes(bytes).map_err(|_| Error::Internal)?;
    if snapshot.digest() != digest {
        return Err(Error::Internal);
    }
    Ok(snapshot)
}

#[derive(Clone, Copy, PartialEq)]
enum Lock {
    None,
    /// Held by an attempt that pins a snapshot: excludes changes, not other pins.
    Share,
    /// Held by every change; serializes them.
    Update,
}
async fn read_settings(conn: &mut PgConnection, lock: Lock) -> Result<Settings> {
    const COLUMNS: &str = "revision,active_digest,selection_defaults,selection_defaults_revision,limits,limits_revision,routing_markdown,routing_revision";
    #[cfg(feature = "delivery_playbook")]
    const DELIVERY: &str =
        ",delivery_markdown,delivery_revision,delivery_limits,delivery_limits_revision";
    #[cfg(not(feature = "delivery_playbook"))]
    const DELIVERY: &str = "";
    let suffix = match lock {
        Lock::None => "",
        Lock::Share => " FOR SHARE",
        Lock::Update => " FOR UPDATE",
    };
    let sql = format!("SELECT {COLUMNS}{DELIVERY} FROM role_settings WHERE singleton{suffix}");
    let row = sqlx::query(&sql).fetch_one(conn).await?;
    // NULL: never saved, so the defaults of this build apply.
    fn stored<T: serde::de::DeserializeOwned + Default>(
        value: Option<serde_json::Value>,
    ) -> Result<T> {
        match value {
            Some(value) => serde_json::from_value(value).map_err(|_| Error::Internal),
            None => Ok(T::default()),
        }
    }
    Ok(Settings {
        revision: row.get("revision"),
        active_digest: row.get("active_digest"),
        selection_defaults: stored(row.get("selection_defaults"))?,
        selection_defaults_revision: row.get("selection_defaults_revision"),
        limits: stored(row.get("limits"))?,
        limits_revision: row.get("limits_revision"),
        routing_markdown: row
            .get::<Option<String>, _>("routing_markdown")
            .unwrap_or_default(),
        routing_revision: row.get("routing_revision"),
        #[cfg(feature = "delivery_playbook")]
        delivery_markdown: row
            .get::<Option<String>, _>("delivery_markdown")
            .unwrap_or_default(),
        #[cfg(feature = "delivery_playbook")]
        delivery_revision: row.get("delivery_revision"),
        #[cfg(feature = "delivery_playbook")]
        delivery_limits: stored(row.get("delivery_limits"))?,
        #[cfg(feature = "delivery_playbook")]
        delivery_limits_revision: row.get("delivery_limits_revision"),
    })
}
async fn documents(conn: &mut PgConnection, kind: Option<Kind>) -> Result<Vec<Document>> {
    sqlx::query(
        "SELECT kind,id,markdown,revision,updated_at_ms FROM role_documents WHERE $1::text IS NULL OR kind=$1 ORDER BY kind,id",
    )
    .bind(kind.map(kind_name))
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(|row| {
        Ok(Document {
            kind: kind_of(row.get("kind"))?,
            id: row.get("id"),
            markdown: row.get("markdown"),
            revision: row.get("revision"),
            updated_at_ms: row.get("updated_at_ms"),
        })
    })
    .collect()
}
fn compile(
    documents: &[Document],
    defaults: &SelectionDefaults,
    limits: &StoreLimits,
) -> std::result::Result<Snapshot, Vec<Problem>> {
    let sources: Vec<_> = documents
        .iter()
        .map(|d| SourceDocument {
            kind: d.kind,
            label: d.id.clone(),
            text: d.markdown.clone(),
        })
        .collect();
    roles::compile(&sources, defaults, &limits.documents)
}
/// The id a document states, when it parses.
fn stated_id(kind: Kind, markdown: &str) -> Option<String> {
    match kind {
        Kind::Role => roles::parse_role(markdown).ok().map(|d| d.id),
        Kind::Intelligence => roles::parse_intelligence(markdown).ok().map(|d| d.id),
        Kind::Execution => roles::parse_execution(markdown).ok().map(|d| d.id),
    }
}

/// The documents of one kind, in id order.
pub async fn list(pool: &PgPool, kind: Kind) -> Result<Vec<Document>> {
    documents(&mut *pool.acquire().await?, Some(kind)).await
}
pub async fn get(pool: &PgPool, kind: Kind, id: &str) -> Result<Option<Document>> {
    Ok(list(pool, kind).await?.into_iter().find(|d| d.id == id))
}
/// The current selection defaults, read on `conn`. A store without the Settings table (held at
/// an earlier migration) or without a saved value has the values a new installation starts with.
pub async fn selection_defaults(conn: &mut PgConnection) -> Result<SelectionDefaults> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('role_settings') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(SelectionDefaults::default());
    }
    let stored: Option<Option<serde_json::Value>> =
        sqlx::query_scalar("SELECT selection_defaults FROM role_settings WHERE singleton")
            .fetch_optional(conn)
            .await?;
    match stored.flatten() {
        Some(value) => serde_json::from_value(value).map_err(|_| Error::Internal),
        None => Ok(SelectionDefaults::default()),
    }
}
pub async fn settings(pool: &PgPool) -> Result<Settings> {
    read_settings(&mut *pool.acquire().await?, Lock::None).await
}
async fn read_snapshot(conn: &mut PgConnection, digest: &str) -> Result<Option<Snapshot>> {
    let bytes: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT snapshot FROM role_snapshots WHERE digest=$1")
            .bind(digest)
            .fetch_optional(conn)
            .await?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    stored_snapshot(&bytes, digest).map(Some)
}
/// A stored snapshot: the active one, one an attempt pins, or one of the
/// most recent others. `None` once it has rolled off.
pub async fn snapshot(pool: &PgPool, digest: &str) -> Result<Option<Snapshot>> {
    read_snapshot(&mut *pool.acquire().await?, digest).await
}
/// The snapshot new work launches with. `None` until a valid set was stored.
pub async fn active_snapshot(pool: &PgPool) -> Result<Option<Active>> {
    // One statement, so the revision and the snapshot belong together.
    let row = sqlx::query(
        "SELECT r.revision,s.digest,s.snapshot FROM role_settings r JOIN role_snapshots s ON s.digest=r.active_digest WHERE r.singleton",
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let snapshot = stored_snapshot(
        &row.get::<Vec<u8>, _>("snapshot"),
        &row.get::<String, _>("digest"),
    )?;
    Ok(Some(Active {
        revision: row.get("revision"),
        snapshot,
    }))
}

/// Rolling snapshots (spec 0014 §7.1): keep the active one, every one an
/// attempt pins, and the `snapshots_kept` most recently activated others.
async fn roll_snapshots(conn: &mut PgConnection, active: &str, kept: u32) -> Result<()> {
    sqlx::query(
        "DELETE FROM role_snapshots s WHERE s.digest<>$1
         AND NOT EXISTS(SELECT 1 FROM role_snapshot_pins p WHERE p.digest=s.digest)
         AND s.digest NOT IN (SELECT o.digest FROM role_snapshots o WHERE o.digest<>$1
           AND NOT EXISTS(SELECT 1 FROM role_snapshot_pins p WHERE p.digest=o.digest)
           ORDER BY o.activated_revision DESC LIMIT $2)",
    )
    .bind(active)
    .bind(i64::from(kept))
    .execute(conn)
    .await?;
    Ok(())
}
/// Pins are for live attempts (spec 0014 §7.1): release the pin of every
/// attempt that no longer holds its task (settled, cancelled, abandoned, lost
/// and released, never granted) or whose record is gone. Idempotent. It runs
/// when an attempt is created and before every roll, so pins stay bounded by
/// the live attempts and a finished attempt never keeps a snapshot from
/// rolling off. Its summary (`attempt_ran_with`) stays.
pub async fn release_finished_pins(conn: &mut PgConnection) -> Result<u64> {
    let released = sqlx::query(
        "DELETE FROM role_snapshot_pins p WHERE NOT EXISTS
         (SELECT 1 FROM execution_attempts a WHERE a.attempt_id=p.attempt_id AND a.holds_task)",
    )
    .execute(conn)
    .await?;
    Ok(released.rows_affected())
}
/// Store `snapshot`, make it the active one at `revision`, and roll.
async fn activate(
    conn: &mut PgConnection,
    snapshot: &Snapshot,
    revision: i64,
    limits: &StoreLimits,
    now_ms: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO role_snapshots(digest,snapshot,created_at_ms,activated_revision) VALUES($1,$2,$3,$4)
         ON CONFLICT(digest) DO UPDATE SET activated_revision=EXCLUDED.activated_revision",
    )
    .bind(snapshot.digest())
    .bind(snapshot.canonical_bytes())
    .bind(now_ms)
    .bind(revision)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE role_settings SET revision=$1,active_digest=$2 WHERE singleton")
        .bind(revision)
        .bind(snapshot.digest())
        .execute(&mut *conn)
        .await?;
    release_finished_pins(&mut *conn).await?;
    roll_snapshots(conn, snapshot.digest(), limits.snapshots_kept).await
}

/// Apply one change (see the crate documentation). `Error::Problems` when the
/// resulting set is not valid, `Error::Conflict` when `expected_revision` is
/// stale or the operation id was used for another request.
pub async fn apply(
    pool: &PgPool,
    actor: Uuid,
    mutation: &Mutation,
    now_ms: i64,
) -> Result<Receipt> {
    apply_referenced(pool, actor, mutation, now_ms, None).await
}
/// [`apply`], with the references from outside this store that a delete must
/// respect. They are read in the change's transaction, after its lock, so a
/// reference saved under [`check_binding`] is always seen.
pub async fn apply_referenced(
    pool: &PgPool,
    actor: Uuid,
    mutation: &Mutation,
    now_ms: i64,
    references: Option<References>,
) -> Result<Receipt> {
    if mutation.operation_id.is_nil() {
        return Err(invalid("Enter a valid operation ID"));
    }
    if mutation.expected_revision < 0 {
        return Err(invalid("Invalid revision"));
    }
    let request_digest = aq_profiles::digest(&serde_json::json!({
        "change": mutation.change,
        "expected_revision": mutation.expected_revision,
    }))
    .map_err(|_| Error::Internal)?;
    let mut tx = pool.begin().await?;
    let mut state = read_settings(&mut tx, Lock::Update).await?;
    let replayed = sqlx::query(
        "SELECT actor_id,request_digest,receipt FROM role_settings_operations WHERE operation_id=$1",
    )
    .bind(mutation.operation_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = replayed {
        if row.get::<Uuid, _>("actor_id") != actor
            || row.get::<String, _>("request_digest") != request_digest
        {
            return Err(conflict("Operation ID already used for another request"));
        }
        return serde_json::from_value(row.get("receipt")).map_err(|_| Error::Internal);
    }
    let mut set = documents(&mut tx, None).await?;
    let revision = state.revision + 1;
    let mut problems = Vec::new();
    let current = match &mutation.change {
        Change::Save { kind, id, .. } | Change::Delete { kind, id } => {
            let at = set.iter().position(|d| d.kind == *kind && d.id == *id);
            let current = at.map_or(0, |at| set[at].revision);
            match (&mutation.change, at) {
                (Change::Save { markdown, .. }, _) => {
                    if let Some(stated) = stated_id(*kind, markdown).filter(|stated| stated != id) {
                        problems.push(Problem {
                            document: format!("{} document `{id}`", kind.label()),
                            field: "id".into(),
                            message: format!(
                                "is `{stated}`; a document saved as `{id}` must have that id"
                            ),
                        });
                    }
                    let document = Document {
                        kind: *kind,
                        id: id.clone(),
                        markdown: markdown.clone(),
                        revision,
                        updated_at_ms: now_ms,
                    };
                    match at {
                        Some(at) => set[at] = document,
                        None => set.push(document),
                    }
                }
                (_, Some(at)) => {
                    set.remove(at);
                }
                (_, None) => {
                    return Err(Error::NotFound(format!(
                        "No {} `{id}` exists",
                        kind.label()
                    )));
                }
            }
            current
        }
        Change::SelectionDefaults(defaults) => {
            let current = state.selection_defaults_revision;
            state.selection_defaults = defaults.clone();
            current
        }
        Change::Limits(limits) => {
            let current = state.limits_revision;
            state.limits = *limits;
            current
        }
        Change::RoutingDocument(_) => state.routing_revision,
        Change::ObservationLimits(_) => observation_limits(&mut tx).await?.1,
        #[cfg(feature = "delivery_playbook")]
        Change::DeliveryDocument(_) => state.delivery_revision,
        #[cfg(feature = "delivery_playbook")]
        Change::DeliveryLimits(_) => state.delivery_limits_revision,
        #[cfg(feature = "project_environments")]
        Change::EnvironmentLimits(_) => environment_limits_on(&mut tx).await?.1,
    };
    if current != mutation.expected_revision {
        return Err(conflict(
            "This changed since it was read; reload before saving",
        ));
    }
    if let (Change::Delete { kind, id }, Some(read)) = (&mutation.change, references) {
        for reference in read(&mut tx).await? {
            if reference.kind == *kind && reference.id == *id {
                problems.push(Problem {
                    document: format!("{} `{id}`", kind.label()),
                    field: String::new(),
                    message: format!("is {}", reference.by),
                });
            }
        }
    }
    // The routing document is not part of a snapshot: saving it compiles
    // and activates nothing.
    #[cfg(feature = "delivery_playbook")]
    let delivery = match &mutation.change {
        Change::DeliveryDocument(markdown) => {
            let limit = state.limits.documents.max_document_bytes;
            if markdown.len() > limit {
                return Err(invalid(&format!(
                    "The delivery document is {} bytes, over the limit of {limit} bytes",
                    markdown.len()
                )));
            }
            true
        }
        Change::DeliveryLimits(limits) => {
            limits.validate().map_err(|message| invalid(&message))?;
            true
        }
        _ => false,
    };
    #[cfg(not(feature = "delivery_playbook"))]
    let delivery = false;
    // The environment limits, too, compile and activate nothing.
    #[cfg(feature = "project_environments")]
    let delivery = delivery
        || match &mutation.change {
            Change::EnvironmentLimits(limits) => {
                limits.validate().map_err(|message| invalid(&message))?;
                true
            }
            _ => false,
        };
    let snapshot = if let Change::RoutingDocument(markdown) = &mutation.change {
        let limit = state.limits.documents.max_document_bytes;
        if markdown.len() > limit {
            return Err(invalid(&format!(
                "The routing document is {} bytes, over the limit of {limit} bytes",
                markdown.len()
            )));
        }
        None
    } else if let Change::ObservationLimits(limits) = &mutation.change {
        limits.validate().map_err(|message| invalid(&message))?;
        None
    } else if delivery {
        // Like the routing document, the delivery settings compile and
        // activate nothing.
        None
    } else {
        match compile(&set, &state.selection_defaults, &state.limits) {
            Ok(snapshot) if problems.is_empty() => Some(snapshot),
            Ok(_) => return Err(Error::Problems(problems)),
            Err(more) => {
                problems.extend(more);
                return Err(Error::Problems(problems));
            }
        }
    };
    match &mutation.change {
        Change::Save { kind, id, markdown } => {
            sqlx::query(
                "INSERT INTO role_documents(kind,id,markdown,revision,updated_at_ms) VALUES($1,$2,$3,$4,$5)
                 ON CONFLICT(kind,id) DO UPDATE SET markdown=EXCLUDED.markdown,revision=EXCLUDED.revision,updated_at_ms=EXCLUDED.updated_at_ms",
            )
            .bind(kind_name(*kind))
            .bind(id)
            .bind(markdown)
            .bind(revision)
            .bind(now_ms)
            .execute(&mut *tx)
            .await?;
        }
        Change::Delete { kind, id } => {
            sqlx::query("DELETE FROM role_documents WHERE kind=$1 AND id=$2")
                .bind(kind_name(*kind))
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        Change::SelectionDefaults(defaults) => {
            sqlx::query(
                "UPDATE role_settings SET selection_defaults=$1,selection_defaults_revision=$2 WHERE singleton",
            )
            .bind(serde_json::to_value(defaults).map_err(|_| Error::Internal)?)
            .bind(revision)
            .execute(&mut *tx)
            .await?;
        }
        Change::Limits(limits) => {
            sqlx::query("UPDATE role_settings SET limits=$1,limits_revision=$2 WHERE singleton")
                .bind(serde_json::to_value(limits).map_err(|_| Error::Internal)?)
                .bind(revision)
                .execute(&mut *tx)
                .await?;
        }
        Change::RoutingDocument(markdown) => {
            sqlx::query(
                "UPDATE role_settings SET revision=$1,routing_markdown=$2,routing_revision=$1 WHERE singleton",
            )
            .bind(revision)
            .bind(markdown)
            .execute(&mut *tx)
            .await?;
        }
        Change::ObservationLimits(limits) => {
            sqlx::query(
                "UPDATE role_settings SET revision=$1,observation_limits=$2,observation_limits_revision=$1 WHERE singleton",
            )
            .bind(revision)
            .bind(serde_json::to_value(limits).map_err(|_| Error::Internal)?)
            .execute(&mut *tx)
            .await?;
        }
        #[cfg(feature = "delivery_playbook")]
        Change::DeliveryDocument(markdown) => {
            sqlx::query(
                "UPDATE role_settings SET revision=$1,delivery_markdown=$2,delivery_revision=$1 WHERE singleton",
            )
            .bind(revision)
            .bind(markdown)
            .execute(&mut *tx)
            .await?;
        }
        #[cfg(feature = "delivery_playbook")]
        Change::DeliveryLimits(limits) => {
            sqlx::query(
                "UPDATE role_settings SET revision=$1,delivery_limits=$2,delivery_limits_revision=$1 WHERE singleton",
            )
            .bind(revision)
            .bind(serde_json::to_value(limits).map_err(|_| Error::Internal)?)
            .execute(&mut *tx)
            .await?;
        }
        #[cfg(feature = "project_environments")]
        Change::EnvironmentLimits(limits) => {
            sqlx::query(
                "UPDATE role_settings SET revision=$1,environment_limits=$2,environment_limits_revision=$1 WHERE singleton",
            )
            .bind(revision)
            .bind(serde_json::to_value(limits).map_err(|_| Error::Internal)?)
            .execute(&mut *tx)
            .await?;
        }
    }
    let digest = match &snapshot {
        Some(snapshot) => {
            activate(&mut tx, snapshot, revision, &state.limits, now_ms).await?;
            snapshot.digest().to_owned()
        }
        None => state.active_digest.clone().unwrap_or_default(),
    };
    let receipt = Receipt { revision, digest };
    sqlx::query(
        "INSERT INTO role_settings_operations(operation_id,actor_id,request_digest,receipt,revision,created_at_ms) VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(mutation.operation_id)
    .bind(actor)
    .bind(&request_digest)
    .bind(serde_json::to_value(&receipt).map_err(|_| Error::Internal)?)
    .bind(revision)
    .bind(now_ms)
    .execute(&mut *tx)
    .await?;
    // Receipts are a rolling set too: a repeat is recognised while its id is
    // among the most recent `operations_kept`; later it is a new request.
    sqlx::query(
        "DELETE FROM role_settings_operations WHERE operation_id NOT IN
         (SELECT operation_id FROM role_settings_operations ORDER BY revision DESC LIMIT $1)",
    )
    .bind(i64::from(state.limits.operations_kept))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(receipt)
}
/// Create or replace one document. `expected_revision` is the document's
/// revision as last read, 0 when it does not exist yet.
#[allow(clippy::too_many_arguments)]
pub async fn save(
    pool: &PgPool,
    actor: Uuid,
    kind: Kind,
    id: &str,
    markdown: &str,
    expected_revision: i64,
    operation_id: Uuid,
    now_ms: i64,
) -> Result<Receipt> {
    let change = Change::Save {
        kind,
        id: id.into(),
        markdown: markdown.into(),
    };
    let mutation = Mutation {
        change,
        expected_revision,
        operation_id,
    };
    apply(pool, actor, &mutation, now_ms).await
}
/// Delete one document. Refused with the problems when something still
/// refers to it.
pub async fn delete(
    pool: &PgPool,
    actor: Uuid,
    kind: Kind,
    id: &str,
    expected_revision: i64,
    operation_id: Uuid,
    now_ms: i64,
) -> Result<Receipt> {
    let change = Change::Delete {
        kind,
        id: id.into(),
    };
    let mutation = Mutation {
        change,
        expected_revision,
        operation_id,
    };
    apply(pool, actor, &mutation, now_ms).await
}
pub async fn save_selection_defaults(
    pool: &PgPool,
    actor: Uuid,
    defaults: &SelectionDefaults,
    expected_revision: i64,
    operation_id: Uuid,
    now_ms: i64,
) -> Result<Receipt> {
    let mutation = Mutation {
        change: Change::SelectionDefaults(defaults.clone()),
        expected_revision,
        operation_id,
    };
    apply(pool, actor, &mutation, now_ms).await
}
/// Replace the routing document (spec 0014 §5.2: it has its own place in
/// Settings). `expected_revision` is its revision as last read, 0 before the
/// first save.
pub async fn save_routing_document(
    pool: &PgPool,
    actor: Uuid,
    markdown: &str,
    expected_revision: i64,
    operation_id: Uuid,
    now_ms: i64,
) -> Result<Receipt> {
    let mutation = Mutation {
        change: Change::RoutingDocument(markdown.into()),
        expected_revision,
        operation_id,
    };
    apply(pool, actor, &mutation, now_ms).await
}
/// The routing document and its revision; empty at revision 0 until saved.
pub async fn routing_document(pool: &PgPool) -> Result<(String, i64)> {
    let settings = settings(pool).await?;
    Ok((settings.routing_markdown, settings.routing_revision))
}
/// The Settings document a built-in playbook source is authored from, with its
/// revision; `None` for a source that has none.
pub async fn document_of(pool: &PgPool, source: &str) -> Result<Option<(String, i64)>> {
    match source {
        "routing" => routing_document(pool).await.map(Some),
        #[cfg(feature = "delivery_playbook")]
        "delivery" => delivery_document(pool).await.map(Some),
        _ => Ok(None),
    }
}
/// Replace the delivery document (spec 0016 §8). `expected_revision` is its
/// revision as last read, 0 before the first save.
#[cfg(feature = "delivery_playbook")]
pub async fn save_delivery_document(
    pool: &PgPool,
    actor: Uuid,
    markdown: &str,
    expected_revision: i64,
    operation_id: Uuid,
    now_ms: i64,
) -> Result<Receipt> {
    let mutation = Mutation {
        change: Change::DeliveryDocument(markdown.into()),
        expected_revision,
        operation_id,
    };
    apply(pool, actor, &mutation, now_ms).await
}
/// The delivery document and its revision; empty at revision 0 until saved,
/// which means no delivery policy.
#[cfg(feature = "delivery_playbook")]
pub async fn delivery_document(pool: &PgPool) -> Result<(String, i64)> {
    let settings = settings(pool).await?;
    Ok((settings.delivery_markdown, settings.delivery_revision))
}
/// Replace the delivery limits. They are validated
/// ([`delivery::DeliveryLimits::validate`]) before anything is stored.
#[cfg(feature = "delivery_playbook")]
pub async fn save_delivery_limits(
    pool: &PgPool,
    actor: Uuid,
    limits: &delivery::DeliveryLimits,
    expected_revision: i64,
    operation_id: Uuid,
    now_ms: i64,
) -> Result<Receipt> {
    let mutation = Mutation {
        change: Change::DeliveryLimits(*limits),
        expected_revision,
        operation_id,
    };
    apply(pool, actor, &mutation, now_ms).await
}
/// The delivery limits and their revision; the defaults at revision 0 until
/// saved.
#[cfg(feature = "delivery_playbook")]
pub async fn delivery_limits(pool: &PgPool) -> Result<(delivery::DeliveryLimits, i64)> {
    let settings = settings(pool).await?;
    Ok((settings.delivery_limits, settings.delivery_limits_revision))
}
/// The node observation limits and their revision (the defaults at revision
/// 0 until first saved), read on the caller's connection: a cap check reads
/// them inside its own transaction, under the locks it already holds. They
/// are read apart from the other Settings values, whose one read must keep
/// working on a store whose role tables predate these columns.
pub async fn observation_limits(
    conn: &mut PgConnection,
) -> sqlx::Result<(crate::observation::ObservationLimits, i64)> {
    let (stored, revision): (Option<serde_json::Value>, i64) = sqlx::query_as(
        "SELECT observation_limits,observation_limits_revision FROM role_settings WHERE singleton",
    )
    .fetch_one(conn)
    .await?;
    let limits = match stored {
        Some(value) => serde_json::from_value(value).map_err(|e| sqlx::Error::Decode(e.into()))?,
        None => Default::default(),
    };
    Ok((limits, revision))
}
/// The environment limits and their revision (spec 0017 §6.7); the defaults
/// at revision 0 until first saved.
#[cfg(feature = "project_environments")]
pub async fn environment_limits(
    pool: &PgPool,
) -> Result<(crate::environments::EnvironmentLimits, i64)> {
    environment_limits_on(&mut *pool.acquire().await?).await
}
#[cfg(feature = "project_environments")]
async fn environment_limits_on(
    conn: &mut PgConnection,
) -> Result<(crate::environments::EnvironmentLimits, i64)> {
    let (stored, revision): (Option<serde_json::Value>, i64) = sqlx::query_as(
        "SELECT environment_limits,environment_limits_revision FROM role_settings WHERE singleton",
    )
    .fetch_one(conn)
    .await?;
    let limits = match stored {
        Some(value) => serde_json::from_value(value).map_err(|_| Error::Internal)?,
        None => Default::default(),
    };
    Ok((limits, revision))
}
/// Replace the environment limits; they are validated before anything is stored.
#[cfg(feature = "project_environments")]
pub async fn save_environment_limits(
    pool: &PgPool,
    actor: Uuid,
    limits: &crate::environments::EnvironmentLimits,
    expected_revision: i64,
    operation_id: Uuid,
    now_ms: i64,
) -> Result<Receipt> {
    let mutation = Mutation {
        change: Change::EnvironmentLimits(*limits),
        expected_revision,
        operation_id,
    };
    apply(pool, actor, &mutation, now_ms).await
}
pub async fn save_limits(
    pool: &PgPool,
    actor: Uuid,
    limits: &StoreLimits,
    expected_revision: i64,
    operation_id: Uuid,
    now_ms: i64,
) -> Result<Receipt> {
    let mutation = Mutation {
        change: Change::Limits(*limits),
        expected_revision,
        operation_id,
    };
    apply(pool, actor, &mutation, now_ms).await
}

/// Install each shipped default document that was never installed in this
/// database (`role_default_installs`). It happens once per document: one the
/// owner deletes does not come back at the next start, and a default added
/// in a later release installs at the first start of that release. An
/// existing document is never overwritten. Returns what was added.
pub async fn install_defaults(pool: &PgPool, now_ms: i64) -> Result<Vec<(Kind, String)>> {
    let mut tx = pool.begin().await?;
    let state = read_settings(&mut tx, Lock::Update).await?;
    let revision = state.revision + 1;
    let mut installed = Vec::new();
    for document in roles::default_documents() {
        let id = stated_id(document.kind, &document.text).ok_or(Error::Internal)?;
        let first = sqlx::query(
            "INSERT INTO role_default_installs(kind,id,installed_at_ms) VALUES($1,$2,$3) ON CONFLICT(kind,id) DO NOTHING",
        )
        .bind(kind_name(document.kind))
        .bind(&id)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if first == 0 {
            continue;
        }
        let added = sqlx::query(
            "INSERT INTO role_documents(kind,id,markdown,revision,updated_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(kind,id) DO NOTHING",
        )
        .bind(kind_name(document.kind))
        .bind(&id)
        .bind(&document.text)
        .bind(revision)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if added == 1 {
            installed.push((document.kind, id));
        }
    }
    if !installed.is_empty() {
        sqlx::query("UPDATE role_settings SET revision=$1 WHERE singleton")
            .bind(revision)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(installed)
}
/// Compile the stored set and make its snapshot the active one when it is
/// not already. Returns the active digest and, when the stored set does not
/// compile, its problems; the active snapshot is then left as it was.
pub async fn activate_stored(pool: &PgPool, now_ms: i64) -> Result<(Option<String>, Vec<Problem>)> {
    let mut tx = pool.begin().await?;
    let state = read_settings(&mut tx, Lock::Update).await?;
    let set = documents(&mut tx, None).await?;
    let snapshot = match compile(&set, &state.selection_defaults, &state.limits) {
        Ok(snapshot) => snapshot,
        Err(problems) => return Ok((state.active_digest, problems)),
    };
    if state.active_digest.as_deref() != Some(snapshot.digest()) {
        activate(
            &mut tx,
            &snapshot,
            state.revision + 1,
            &state.limits,
            now_ms,
        )
        .await?;
        tx.commit().await?;
    }
    Ok((Some(snapshot.digest().into()), Vec::new()))
}
/// Server start: install the defaults never installed before, then make sure the stored set's
/// snapshot is the active one. An invalid stored set is reported, not fatal.
pub async fn startup(pool: &PgPool, now_ms: i64) -> Result<Startup> {
    let installed = install_defaults(pool, now_ms).await?;
    let (active_digest, problems) = activate_stored(pool, now_ms).await?;
    Ok(Startup {
        installed,
        active_digest,
        problems,
    })
}

/// Record that the attempt `attempt_id` launched with snapshot `digest`, so
/// the snapshot stays while the attempt holds its task
/// ([`release_finished_pins`]). Call it in the transaction that creates the
/// attempt. Repeating it is harmless.
pub async fn pin(conn: &mut PgConnection, digest: &str, attempt_id: Uuid) -> Result<()> {
    read_settings(&mut *conn, Lock::Share).await?;
    let pinned = sqlx::query(
        "INSERT INTO role_snapshot_pins(attempt_id,digest) SELECT $1,digest FROM role_snapshots WHERE digest=$2
         ON CONFLICT(attempt_id) DO UPDATE SET digest=EXCLUDED.digest",
    )
    .bind(attempt_id)
    .bind(digest)
    .execute(conn)
    .await?
    .rows_affected();
    if pinned == 0 {
        return Err(Error::NotFound("That snapshot is no longer stored".into()));
    }
    Ok(())
}
/// The attempt's record is gone: release its snapshot, and delete it if
/// nothing else keeps it. Call it in the transaction that removes the record.
pub async fn unpin(conn: &mut PgConnection, attempt_id: Uuid) -> Result<()> {
    let state = read_settings(&mut *conn, Lock::Update).await?;
    sqlx::query("DELETE FROM role_snapshot_pins WHERE attempt_id=$1")
        .bind(attempt_id)
        .execute(&mut *conn)
        .await?;
    if let Some(active) = &state.active_digest {
        roll_snapshots(conn, active, state.limits.snapshots_kept).await?;
    }
    Ok(())
}

/// Check a project binding's defaults and role notes against the active
/// snapshot and this store's document byte limit ([`tasks::check_binding`]),
/// in the transaction that saves the binding. The lock it takes is held
/// until that transaction ends, so a delete of a named document waits and
/// then sees the binding among its [`References`].
pub async fn check_binding(
    conn: &mut PgConnection,
    default_role: &str,
    default_intelligence: &str,
    role_notes: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let state = read_settings(&mut *conn, Lock::Share).await?;
    let snapshot = match &state.active_digest {
        Some(digest) => read_snapshot(conn, digest).await?,
        None => None,
    };
    crate::tasks::check_binding(
        snapshot.as_ref(),
        default_role,
        default_intelligence,
        role_notes,
        state.limits.documents.max_document_bytes,
    )
    .map_err(Error::Invalid)
}
