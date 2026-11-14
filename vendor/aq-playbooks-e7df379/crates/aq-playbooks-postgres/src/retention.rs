use crate::backend;
use crate::profiling::ObservedTransaction;
use aq_playbooks_model::{canonical_bytes, hash_bytes};
use aq_playbooks_runner::retention::Removals;
use aq_playbooks_runner::{State, StorageError};

fn record(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    kind: &str,
    key: &str,
    digest: Option<&str>,
    at: i64,
) -> Result<(), StorageError> {
    if let Some(old)=tx.query_opt("SELECT content_digest FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind=$2 AND identity=$3",&[&namespace,&kind,&key]).map_err(backend)?
        && old.get::<_,Option<String>>(0).as_deref()!=digest {
        return Err(backend("disposal identity content conflict"));
    }
    tx.execute("INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",&[&namespace,&kind,&key,&digest,&at]).map_err(backend)?;
    Ok(())
}
/// Retention GC cutoff deletes for `next`'s GC clock (shared with format 3).
pub(crate) fn collect(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    next: &State,
) -> Result<(), StorageError> {
    if let Some(now) = next.retention.last_gc_at_ms {
        let cutoff =
            now.saturating_sub(i64::try_from(next.retention.policy.audit_ms).unwrap_or(i64::MAX));
        tx.execute("WITH candidates AS MATERIALIZED(SELECT c.digest FROM aq_playbooks.artifact_catalog c JOIN aq_playbooks.disposal_tombstones t ON t.identity=c.digest AND t.kind='artifact' WHERE t.namespace=$1 AND t.disposed_at_ms<$2 AND NOT EXISTS(SELECT 1 FROM aq_playbooks.namespace_artifacts n WHERE n.digest=c.digest) AND NOT EXISTS(SELECT 1 FROM aq_playbooks.artifacts a WHERE a.digest=c.digest) AND NOT EXISTS(SELECT 1 FROM aq_playbooks.encrypted_artifacts e WHERE e.digest=c.digest) ORDER BY c.digest LIMIT 1000 FOR UPDATE OF c SKIP LOCKED) DELETE FROM aq_playbooks.artifact_catalog c USING candidates x WHERE c.digest=x.digest",&[&namespace,&cutoff]).map_err(backend)?;
        tx.execute("DELETE FROM aq_playbooks.disposal_tombstones t WHERE namespace=$1 AND disposed_at_ms<$2 AND ((kind='run' AND NOT EXISTS(SELECT 1 FROM aq_playbooks.inspection_rows r WHERE r.namespace=t.namespace AND r.kind='runs' AND r.item_key=t.identity)) OR (kind='intent' AND NOT EXISTS(SELECT 1 FROM aq_playbooks.inspection_rows r WHERE r.namespace=t.namespace AND r.kind='effects' AND r.item_key=t.identity)) OR (kind='artifact' AND NOT EXISTS(SELECT 1 FROM aq_playbooks.artifact_catalog c WHERE c.digest=t.identity)))",&[&namespace,&cutoff]).map_err(backend)?;
    }
    Ok(())
}
/// Durable side of the shared removal check. `removals` is what
/// `retention::verify_removals` allowed for this commit: only its legacy
/// removals (and artifact disposals) write tombstone rows. A removal allowed
/// by its history or by an expired window entry writes none.
pub(crate) fn verify_and_record(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    previous: &State,
    next: &State,
    removals: &Removals,
) -> Result<(), StorageError> {
    if next.sequence < previous.sequence {
        return Err(backend("namespace invocation sequence cannot decrease"));
    }
    if previous
        .retention
        .last_gc_at_ms
        .is_some_and(|old| next.retention.last_gc_at_ms.is_none_or(|new| new < old))
    {
        return Err(backend("retention GC clock cannot decrease"));
    }
    for (producer, epoch) in &previous.retention.producer_epochs {
        if next
            .retention
            .producer_epochs
            .get(producer)
            .copied()
            .unwrap_or(0)
            < *epoch
        {
            return Err(backend("producer replay epoch cannot decrease"));
        }
    }
    // Event tombstones may always be removed; their rows go with them.
    let retired: Vec<&String> = previous
        .retention
        .event_tombstones
        .keys()
        .filter(|id| !next.retention.event_tombstones.contains_key(*id))
        .collect();
    forget_event_tombstones(tx, namespace, &retired)?;
    // The cutoff deletes scan the namespace's tombstone rows: run them only
    // in a commit that moves the GC clock (a GC), as the format 3 bounded
    // path does, not on every full commit.
    if next.retention.last_gc_at_ms != previous.retention.last_gc_at_ms {
        collect(tx, namespace, next)?;
    }
    // Only identities this commit creates can collide with a tombstone.
    let mut kinds: Vec<&str> = vec![];
    let mut ids: Vec<&String> = vec![];
    let created = [
        (
            "run",
            next.runs
                .keys()
                .filter(|id| !previous.runs.contains_key(*id))
                .collect::<Vec<_>>(),
        ),
        (
            "intent",
            next.intents
                .keys()
                .filter(|id| !previous.intents.contains_key(*id))
                .collect(),
        ),
        (
            "event",
            next.events
                .keys()
                .filter(|id| !previous.events.contains_key(*id))
                .collect(),
        ),
        (
            "artifact",
            next.artifacts
                .keys()
                .filter(|id| !previous.artifacts.contains_key(*id))
                .collect(),
        ),
    ];
    for (kind, identities) in &created {
        for id in identities {
            kinds.push(kind);
            ids.push(id);
        }
    }
    if !ids.is_empty() {
        for row in tx.query("SELECT kind,identity,content_digest FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND (kind,identity) IN (SELECT * FROM unnest($2::text[],$3::text[]))",&[&namespace,&kinds,&ids]).map_err(backend)? {
            let kind: String = row.get(0);
            let id: String = row.get(1);
            match kind.as_str() {
                "event" => return Err(backend("disposed event identity cannot be reused")),
                "run" => return Err(backend("disposed run identity cannot be reused")),
                "intent" => return Err(backend("disposed operation identity cannot be reused")),
                "artifact" => {
                    let bytes = canonical_bytes(&next.artifacts[&id]).map_err(backend)?;
                    if row.get::<_, Option<String>>(2).as_deref() != Some(hash_bytes(&bytes).as_str()) {
                        return Err(backend("disposed artifact address content conflict"));
                    }
                }
                _ => {}
            }
        }
    }
    for id in &removals.legacy_events {
        let tombstone = &next.retention.event_tombstones[id];
        record(
            tx,
            namespace,
            "event",
            id,
            Some(&previous.events[id].digest),
            tombstone.disposed_at_ms,
        )?;
    }
    let disposed_at = |run: &str| {
        next.retention
            .disposals
            .iter()
            .find(|d| d.kind == "run" && d.key == run)
            .map(|d| d.at_ms)
            .ok_or_else(|| backend("run removal requires explicit disposal"))
    };
    for id in &removals.legacy_runs {
        record(tx, namespace, "run", id, None, disposed_at(id)?)?;
    }
    for id in &removals.legacy_intents {
        let intent = &previous.intents[id];
        record(
            tx,
            namespace,
            "intent",
            id,
            Some(&intent.input_digest),
            disposed_at(&intent.run)?,
        )?;
    }
    for (id, artifact) in &previous.artifacts {
        if !next.artifacts.contains_key(id) {
            let at = *next
                .retention
                .artifact_disposals
                .get(id)
                .ok_or_else(|| backend("artifact removal requires explicit disposal"))?;
            let bytes = canonical_bytes(artifact).map_err(backend)?;
            record(tx, namespace, "artifact", id, Some(&hash_bytes(&bytes)), at)?;
        }
    }
    // No tombstone count: `max_tombstones` is accepted and ignored, and
    // legacy rows beyond it (drained later) never refuse a commit.
    Ok(())
}
/// Delete the tombstone rows of event tombstones the State no longer holds.
pub(crate) fn forget_event_tombstones(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    ids: &[&String],
) -> Result<(), StorageError> {
    if !ids.is_empty() {
        tx.execute("DELETE FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind='event' AND identity=ANY($2)",&[&namespace,&ids]).map_err(backend)?;
    }
    Ok(())
}

pub(crate) fn dispose_artifacts(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    previous: &State,
    next: &State,
) -> Result<(), StorageError> {
    for id in previous
        .artifacts
        .keys()
        .filter(|id| !next.artifacts.contains_key(*id))
    {
        // Lock common digest metadata to serialize GC with another namespace's
        // installation before checking its shared retention references.
        tx.query_one(
            "SELECT digest FROM aq_playbooks.artifact_catalog WHERE digest=$1 FOR UPDATE",
            &[&id],
        )
        .map_err(backend)?;
        tx.execute(
            "DELETE FROM aq_playbooks.namespace_artifacts WHERE namespace=$1 AND digest=$2",
            &[&namespace, &id],
        )
        .map_err(backend)?;
        tx.execute(
            "DELETE FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",
            &[&namespace, &id],
        )
        .map_err(backend)?;
        tx.execute("DELETE FROM aq_playbooks.artifacts a WHERE digest=$1 AND NOT EXISTS(SELECT 1 FROM aq_playbooks.namespace_artifacts n WHERE n.digest=a.digest)",&[&id]).map_err(backend)?;
    }
    Ok(())
}
