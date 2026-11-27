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
    const DELIVERY: &str =
        ",delivery_markdown,delivery_revision,delivery_limits,delivery_limits_revision";
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
        selection_defaults: stored_selection_defaults(row.get("selection_defaults"))?,
        selection_defaults_revision: row.get("selection_defaults_revision"),
        limits: stored(row.get("limits"))?,
        limits_revision: row.get("limits_revision"),
        routing_markdown: row
            .get::<Option<String>, _>("routing_markdown")
            .unwrap_or_default(),
        routing_revision: row.get("routing_revision"),
        delivery_markdown: row
            .get::<Option<String>, _>("delivery_markdown")
            .unwrap_or_default(),
        delivery_revision: row.get("delivery_revision"),
        delivery_limits: stored(row.get("delivery_limits"))?,
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
    stored_selection_defaults(stored.flatten())
}
/// The selection defaults as stored. A document saved before the lineage time
/// budgets were removed still has their keys; reading leaves them out, and the
/// next save writes the document without them.
fn stored_selection_defaults(value: Option<serde_json::Value>) -> Result<SelectionDefaults> {
    let Some(mut value) = value else {
        return Ok(SelectionDefaults::default());
    };
    roles::drop_removed_keys(&mut value);
    serde_json::from_value(value).map_err(|_| Error::Internal)
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
#[derive(Clone, Copy)]
struct Version {
    revision: i64,
    now_ms: i64,
}

/// Store `snapshot`, make it the active one at `revision`, and roll.
async fn activate(
    conn: &mut PgConnection,
    snapshot: &Snapshot,
    version: Version,
    limits: &StoreLimits,
) -> Result<()> {
    let Version { revision, now_ms } = version;
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

mod mutation;
pub use mutation::{apply, apply_referenced};

/// Create or replace one document. `expected_revision` is the document's
/// revision as last read, 0 when it does not exist yet.
pub async fn save(edit: Edit<'_>, kind: Kind, id: &str, markdown: &str) -> Result<Receipt> {
    let change = Change::Save {
        kind,
        id: id.into(),
        markdown: markdown.into(),
    };
    edit.apply(change).await
}
/// Delete one document. Refused with the problems when something still
/// refers to it.
pub async fn delete(edit: Edit<'_>, kind: Kind, id: &str) -> Result<Receipt> {
    let change = Change::Delete {
        kind,
        id: id.into(),
    };
    edit.apply(change).await
}
pub async fn save_selection_defaults(
    edit: Edit<'_>,
    defaults: &SelectionDefaults,
) -> Result<Receipt> {
    edit.apply(Change::SelectionDefaults(defaults.clone()))
        .await
}
/// Replace the routing document (spec 0014 §5.2: it has its own place in
/// Settings). `expected_revision` is its revision as last read, 0 before the
/// first save.
pub async fn save_routing_document(edit: Edit<'_>, markdown: &str) -> Result<Receipt> {
    edit.apply(Change::RoutingDocument(markdown.into())).await
}
/// The routing document and its revision; empty at revision 0 until saved.
pub async fn routing_document(pool: &PgPool) -> Result<(String, i64)> {
    let settings = settings(pool).await?;
    Ok((settings.routing_markdown, settings.routing_revision))
}
/// Replace the project supervisor document (its role text). `expected_revision`
/// is its revision as last read, 0 before the first save.
pub async fn save_supervisor_document(edit: Edit<'_>, markdown: &str) -> Result<Receipt> {
    edit.apply(Change::SupervisorDocument(markdown.into()))
        .await
}
/// The project supervisor document and its revision; empty at revision 0
/// until saved, which means the supervisor's compiled role text. Read apart
/// from the other Settings values, as the observation limits are.
pub async fn supervisor_document(conn: &mut PgConnection) -> sqlx::Result<(String, i64)> {
    let (markdown, revision): (Option<String>, i64) = sqlx::query_as(
        "SELECT supervisor_markdown,supervisor_revision FROM role_settings WHERE singleton",
    )
    .fetch_one(conn)
    .await?;
    Ok((markdown.unwrap_or_default(), revision))
}
/// The Settings document a built-in playbook source is authored from, with its
/// revision; `None` for a source that has none.
pub async fn document_of(pool: &PgPool, source: &str) -> Result<Option<(String, i64)>> {
    match source {
        "routing" => routing_document(pool).await.map(Some),
        "delivery" => delivery_document(pool).await.map(Some),
        _ => Ok(None),
    }
}
/// Replace the delivery document (spec 0016 §8). `expected_revision` is its
/// revision as last read, 0 before the first save.
pub async fn save_delivery_document(edit: Edit<'_>, markdown: &str) -> Result<Receipt> {
    edit.apply(Change::DeliveryDocument(markdown.into())).await
}
/// The delivery document and its revision; empty at revision 0 until saved,
/// which means no delivery policy.
pub async fn delivery_document(pool: &PgPool) -> Result<(String, i64)> {
    let settings = settings(pool).await?;
    Ok((settings.delivery_markdown, settings.delivery_revision))
}
/// Replace the delivery limits. They are validated
/// ([`delivery::DeliveryLimits::validate`]) before anything is stored.
pub async fn save_delivery_limits(
    edit: Edit<'_>,
    limits: &delivery::DeliveryLimits,
) -> Result<Receipt> {
    edit.apply(Change::DeliveryLimits(
        delivery::DeliveryLimitsChange::replace(*limits),
    ))
    .await
}
/// The delivery limits and their revision; the defaults at revision 0 until
/// saved.
pub async fn delivery_limits(pool: &PgPool) -> Result<(delivery::DeliveryLimits, i64)> {
    let settings = settings(pool).await?;
    Ok((settings.delivery_limits, settings.delivery_limits_revision))
}
/// The node limits and their revision (the defaults at revision 0 until first
/// saved), read on the caller's connection, apart from the other Settings values.
pub async fn node_limits(conn: &mut PgConnection) -> sqlx::Result<(crate::node::NodeLimits, i64)> {
    let (stored, revision): (Option<serde_json::Value>, i64) = sqlx::query_as(
        "SELECT node_limits,node_limits_revision FROM role_settings WHERE singleton",
    )
    .fetch_one(conn)
    .await?;
    let limits = match stored {
        Some(value) => serde_json::from_value(value).map_err(|e| sqlx::Error::Decode(e.into()))?,
        None => Default::default(),
    };
    Ok((limits, revision))
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
        Some(value) => serde_json::from_value(value).map_err(|e| sqlx::Error::Decode(e.into()))?,
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
pub async fn save_limits(edit: Edit<'_>, limits: &StoreLimits) -> Result<Receipt> {
    edit.apply(Change::Limits(*limits)).await
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
            Version {
                revision: state.revision + 1,
                now_ms,
            },
            &state.limits,
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
        crate::tasks::BindingDefaults {
            role: default_role,
            intelligence: default_intelligence,
        },
        role_notes,
        state.limits.documents.max_document_bytes,
    )
    .map_err(Error::Invalid)
}
