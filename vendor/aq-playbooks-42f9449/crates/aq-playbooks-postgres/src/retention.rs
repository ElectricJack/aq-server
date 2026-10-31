use crate::backend;
use crate::profiling::ObservedTransaction;
use aq_playbooks_model::{canonical_bytes, hash_bytes};
use aq_playbooks_runner::{IntentStatus, State, StorageError};

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
pub(crate) fn verify_and_record(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    previous: &State,
    next: &State,
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
    for (id, tombstone) in &previous.retention.event_tombstones {
        if !next.retention.event_tombstones.contains_key(id) {
            if next
                .retention
                .producer_epochs
                .get(&tombstone.producer)
                .copied()
                .unwrap_or(0)
                <= tombstone.epoch
            {
                return Err(backend(
                    "event replay tombstone removal requires producer epoch retirement",
                ));
            }
            tx.execute("DELETE FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind='event' AND identity=$2",&[&namespace,id]).map_err(backend)?;
        }
    }
    if let Some(now) = next.retention.last_gc_at_ms {
        let cutoff =
            now.saturating_sub(i64::try_from(next.retention.policy.audit_ms).unwrap_or(i64::MAX));
        tx.execute("WITH candidates AS MATERIALIZED(SELECT c.digest FROM aq_playbooks.artifact_catalog c JOIN aq_playbooks.disposal_tombstones t ON t.identity=c.digest AND t.kind='artifact' WHERE t.namespace=$1 AND t.disposed_at_ms<$2 AND NOT EXISTS(SELECT 1 FROM aq_playbooks.namespace_artifacts n WHERE n.digest=c.digest) AND NOT EXISTS(SELECT 1 FROM aq_playbooks.artifacts a WHERE a.digest=c.digest) AND NOT EXISTS(SELECT 1 FROM aq_playbooks.encrypted_artifacts e WHERE e.digest=c.digest) ORDER BY c.digest LIMIT 1000 FOR UPDATE OF c SKIP LOCKED) DELETE FROM aq_playbooks.artifact_catalog c USING candidates x WHERE c.digest=x.digest",&[&namespace,&cutoff]).map_err(backend)?;
        tx.execute("DELETE FROM aq_playbooks.disposal_tombstones t WHERE namespace=$1 AND disposed_at_ms<$2 AND ((kind='run' AND NOT EXISTS(SELECT 1 FROM aq_playbooks.inspection_rows r WHERE r.namespace=t.namespace AND r.kind='runs' AND r.item_key=t.identity)) OR (kind='intent' AND NOT EXISTS(SELECT 1 FROM aq_playbooks.inspection_rows r WHERE r.namespace=t.namespace AND r.kind='effects' AND r.item_key=t.identity)) OR (kind='artifact' AND NOT EXISTS(SELECT 1 FROM aq_playbooks.artifact_catalog c WHERE c.digest=t.identity)))",&[&namespace,&cutoff]).map_err(backend)?;
    }
    for row in tx.query("SELECT kind,identity,content_digest FROM aq_playbooks.disposal_tombstones WHERE namespace=$1",&[&namespace]).map_err(backend)?{
        let kind:String=row.get(0);let id:String=row.get(1);match kind.as_str(){
            "event" if next.events.contains_key(&id)=>return Err(backend("disposed event identity cannot be reused")),
            "run" if next.runs.contains_key(&id)=>return Err(backend("disposed run identity cannot be reused")),
            "intent" if next.intents.contains_key(&id)=>return Err(backend("disposed operation identity cannot be reused")),
            "artifact"=>{if let Some(artifact)=next.artifacts.get(&id){let bytes=canonical_bytes(artifact).map_err(backend)?;if row.get::<_,Option<String>>(2).as_deref()!=Some(hash_bytes(&bytes).as_str()){return Err(backend("disposed artifact address content conflict"));}}},_=>{}
        }
    }
    for (id, event) in &previous.events {
        if !next.events.contains_key(id) {
            let tombstone = next
                .retention
                .event_tombstones
                .get(id)
                .ok_or_else(|| backend("event removal requires retained replay tombstone"))?;
            if tombstone.digest != event.digest {
                return Err(backend("event disposal digest conflict"));
            }
            record(
                tx,
                namespace,
                "event",
                id,
                Some(&event.digest),
                tombstone.disposed_at_ms,
            )?;
        }
    }
    for (id, run) in &previous.runs {
        if !next.runs.contains_key(id) {
            let disposal = next
                .retention
                .disposals
                .iter()
                .find(|d| d.kind == "run" && d.key == *id)
                .ok_or_else(|| backend("run removal requires explicit disposal"))?;
            if !run.status.terminal()
                || previous.intents.values().any(|intent| {
                    intent.run == *id
                        && !matches!(
                            intent.status,
                            IntentStatus::Completed
                                | IntentStatus::Rejected
                                | IntentStatus::Cancelled
                        )
                })
                || run.children.iter().any(|child| {
                    previous
                        .runs
                        .get(child)
                        .is_some_and(|r| !r.status.terminal())
                })
            {
                return Err(backend("unresolved run obligations cannot be disposed"));
            }
            record(tx, namespace, "run", id, None, disposal.at_ms)?;
        }
    }
    for (id, intent) in &previous.intents {
        if !next.intents.contains_key(id) {
            let disposal = next
                .retention
                .disposals
                .iter()
                .find(|d| d.kind == "run" && d.key == intent.run)
                .ok_or_else(|| backend("operation removal requires owning run disposal"))?;
            if !matches!(
                intent.status,
                IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
            ) {
                return Err(backend("unresolved effect cannot be disposed"));
            }
            record(
                tx,
                namespace,
                "intent",
                id,
                Some(&intent.input_digest),
                disposal.at_ms,
            )?;
        }
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
    let count: i64 = tx
        .query_one(
            "SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1",
            &[&namespace],
        )
        .map_err(backend)?
        .get(0);
    if count as u64 > next.retention.policy.max_tombstones as u64 {
        return Err(backend(
            "durable disposal tombstone quota reached; GC refused without deleting work",
        ));
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
