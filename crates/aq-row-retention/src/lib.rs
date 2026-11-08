//! DEP6 row retention. Each statement deletes at most `BATCH` rows; dependent
//! facts/receipts are removed before their parents. See docs/operations/row-retention.md
//! for the age, authority and idempotency predicates for every table.
use sqlx::{PgPool, Row};
use std::{collections::BTreeMap, time::Duration};
use uuid::Uuid;

pub const BATCH: i64 = 512;
const DAY: i64 = 86_400_000;
pub const INTERVAL: Duration = Duration::from_secs(600);
pub type Report = BTreeMap<&'static str, u64>;

/// A genuinely new Graph observation. Errors and missing tasks retain all rows.
#[async_trait::async_trait]
pub trait TaskState: Send + Sync {
    async fn completed(&self, workspace: Uuid, task: Uuid) -> anyhow::Result<bool>;
}

async fn refresh(pool: &PgPool, tasks: &dyn TaskState, now: i64, batch: i64) -> anyhow::Result<()> {
    // Discover retained scopes incrementally. Already-known scopes rotate by
    // oldest check, so an installation with >512 tasks does not starve later IDs.
    sqlx::query(r#"INSERT INTO retention_tasks(workspace_id,task_id)
      SELECT DISTINCT workspace_id,task_id FROM (
        SELECT (binding->>'workspace_id')::uuid workspace_id,e.task_id FROM agent_launch_bindings b JOIN execution_attempts e USING(attempt_id)
        UNION SELECT (execution_spec#>>'{task,workspace_id}')::uuid,e.task_id FROM profiled_launches l JOIN execution_attempts e USING(attempt_id)
        UNION SELECT (recipient->>'workspace_id')::uuid,(recipient->>'task_id')::uuid FROM mailbox_messages WHERE recipient->>'kind' IN ('task','session')
      ) s WHERE workspace_id IS NOT NULL AND task_id IS NOT NULL
      AND NOT EXISTS(SELECT 1 FROM retention_tasks t WHERE t.workspace_id=s.workspace_id AND t.task_id=s.task_id)
      ORDER BY workspace_id,task_id LIMIT $1 ON CONFLICT DO NOTHING"#)
        .bind(batch).execute(pool).await?;
    let scopes: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT workspace_id,task_id FROM retention_tasks ORDER BY checked_at_ms,workspace_id,task_id LIMIT $1")
        .bind(batch).fetch_all(pool).await?;
    for (workspace, task) in scopes {
        let complete = tasks.completed(workspace, task).await.unwrap_or(false);
        // holds_task is released only by the execution/settlement owner. Status
        // also refuses uncertain/cancelling attempts, including a released fixture.
        sqlx::query(r#"UPDATE retention_tasks t SET checked_at_ms=$3,settled_since_ms=CASE
          WHEN $4 AND NOT EXISTS(SELECT 1 FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id)
            WHERE e.task_id=t.task_id AND (e.holds_task OR COALESCE(h.state->>'status',e.state->>'status','unknown') NOT IN ('finished','declined','never_granted')))
          THEN COALESCE(settled_since_ms,$3) ELSE NULL END
          WHERE workspace_id=$1 AND task_id=$2"#)
          .bind(workspace).bind(task).bind(now).bind(complete).execute(pool).await?;
    }
    Ok(())
}

// $1 = this run's observation time, $2 = age cutoff, $3 = batch size.
// A scope checked by another run or a previous tick never authorizes a delete.
const ELIGIBLE: &str = r#"EXISTS(SELECT 1 FROM retention_tasks t WHERE t.task_id=e.task_id
 AND t.checked_at_ms=$1 AND t.settled_since_ms<$2)
 AND NOT EXISTS(SELECT 1 FROM execution_attempts a LEFT JOIN profiled_authority_hot h USING(attempt_id)
 WHERE a.task_id=e.task_id AND (a.holds_task OR COALESCE(h.state->>'status',a.state->>'status','unknown') NOT IN ('finished','declined','never_granted')))"#;

async fn statement(
    pool: &PgPool,
    query: &str,
    now: i64,
    cutoff: i64,
    batch: i64,
) -> anyhow::Result<u64> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL aq.row_retention='on'")
        .execute(&mut *tx)
        .await?;
    let query =
        format!("WITH retention_parameters AS (SELECT $1::bigint, $2::bigint, $3::bigint) {query}");
    let n = sqlx::query(&query)
        .bind(now)
        .bind(cutoff)
        .bind(batch)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(n)
}

/// One bounded pass. Inject `now` and `tasks` for deterministic database tests.
/// The batch argument may lower the production limit, never raise it.
pub async fn collect(
    pool: &PgPool,
    tasks: &dyn TaskState,
    now: i64,
    batch: i64,
) -> anyhow::Result<Report> {
    anyhow::ensure!(
        now > 0 && (1..=BATCH).contains(&batch),
        "invalid retention bounds"
    );
    refresh(pool, tasks, now, batch).await?;
    let mut report = Report::new();
    // A fence closes old observation ingest before history or canonical call
    // ownership disappears. Watermarks still reject replay of removed packets.
    statement(
        pool,
        &format!(
            r#"INSERT INTO retention_closed_attempts(attempt_id,closed_at_ms)
      SELECT e.attempt_id,$1 FROM execution_attempts e WHERE {ELIGIBLE}
      AND NOT EXISTS(SELECT 1 FROM retention_closed_attempts c WHERE c.attempt_id=e.attempt_id)
      ORDER BY e.attempt_id LIMIT $3 ON CONFLICT DO NOTHING"#
        ),
        now,
        now - 14 * DAY,
        batch,
    )
    .await?;
    // Node locks match ingestion's first lock and serialize counters and fences.
    let nodes: Vec<Uuid> = sqlx::query_scalar(&format!(
        r#"SELECT DISTINCT o.node_id FROM observation_events o
      JOIN execution_attempts e USING(attempt_id) JOIN retention_closed_attempts c USING(attempt_id)
      WHERE o.recorded_at_ms<$2 AND {ELIGIBLE} ORDER BY o.node_id LIMIT $3"#
    ))
    .bind(now)
    .bind(now - 14 * DAY)
    .bind(batch)
    .fetch_all(pool)
    .await?;
    // One node per run bounds the transaction and work even with many nodes.
    if let Some(node) = nodes.first() {
        let mut tx = pool.begin().await?;
        sqlx::query("SET LOCAL statement_timeout='10s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL aq.row_retention='on'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(node)
            .execute(&mut *tx)
            .await?;
        let ids: Vec<Uuid> = sqlx::query_scalar(&format!(r#"SELECT o.event_id FROM observation_events o
          JOIN execution_attempts e USING(attempt_id) JOIN retention_closed_attempts c USING(attempt_id)
          WHERE o.node_id=$4 AND o.recorded_at_ms<$2 AND {ELIGIBLE} ORDER BY o.sequence LIMIT $3"#))
            .bind(now).bind(now-14*DAY).bind(batch).bind(node).fetch_all(&mut *tx).await?;
        let n = sqlx::query("DELETE FROM observation_facts WHERE event_id=ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        report.insert("observation_facts", n);
        let n = sqlx::query("DELETE FROM observation_events WHERE event_id=ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        report.insert("observation_events", n);
        sqlx::query("UPDATE observation_nodes SET event_count=event_count-$2 WHERE node_id=$1")
            .bind(node)
            .bind(n as i64)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    report.entry("observation_events").or_default();
    report.entry("observation_facts").or_default();
    // Canonical heads outlive facts until their original attempt is fenced and
    // every event/fact is gone. Their projections go last.
    for (table, query) in [
        (
            "observation_call_heads",
            format!(
                r#"DELETE FROM observation_call_heads WHERE call_key IN (
          SELECT o.call_key FROM observation_call_heads o JOIN execution_attempts e ON e.attempt_id=o.owner_attempt_id
          JOIN retention_closed_attempts c USING(attempt_id) WHERE {ELIGIBLE}
          AND NOT EXISTS(SELECT 1 FROM observation_facts f WHERE f.call_key=o.call_key)
          AND NOT EXISTS(SELECT 1 FROM observation_events v WHERE v.attempt_id=e.attempt_id)
          ORDER BY o.call_key LIMIT $3)"#
            ),
        ),
        (
            "observation_attempt_projections",
            format!(
                r#"DELETE FROM observation_attempt_projections WHERE attempt_id IN (
          SELECT o.attempt_id FROM observation_attempt_projections o JOIN execution_attempts e USING(attempt_id)
          JOIN retention_closed_attempts c USING(attempt_id) WHERE {ELIGIBLE} AND o.last_recorded_at_ms<$2
          AND NOT EXISTS(SELECT 1 FROM observation_events v WHERE v.attempt_id=e.attempt_id)
          AND NOT EXISTS(SELECT 1 FROM observation_call_heads h WHERE h.owner_attempt_id=e.attempt_id)
          ORDER BY o.attempt_id LIMIT $3)"#
            ),
        ),
        (
            "observation_projections",
            format!(
                r#"DELETE FROM observation_projections WHERE lineage_id IN (
          SELECT o.lineage_id FROM observation_projections o WHERE o.last_recorded_at_ms<$2
          AND EXISTS(SELECT 1 FROM observation_sources s JOIN execution_attempts e USING(attempt_id)
            JOIN retention_closed_attempts c USING(attempt_id) WHERE s.authority#>>'{{binding,lineage_id}}'=o.lineage_id::text AND {ELIGIBLE})
          AND NOT EXISTS(SELECT 1 FROM observation_events v WHERE v.lineage_id=o.lineage_id)
          AND NOT EXISTS(SELECT 1 FROM observation_call_heads h WHERE h.lineage_id=o.lineage_id)
          ORDER BY o.lineage_id LIMIT $3)"#
            ),
        ),
    ] {
        report.insert(
            table,
            statement(pool, &query, now, now - 14 * DAY, batch).await?,
        );
    }
    packets(pool, now, batch, &mut report).await?;
    outboxes(pool, now, batch, &mut report).await?;
    terminals(pool, now, batch, &mut report).await?;
    mail(pool, now, batch, &mut report).await?;
    // Staged owner attachments never bound to a message expire after 24h.
    // Bound rows follow their append-only owner message (ON DELETE CASCADE).
    report.insert(
        "global_supervisor_attachments",
        statement(
            pool,
            "DELETE FROM global_supervisor_attachments WHERE attachment_id IN (SELECT attachment_id FROM global_supervisor_attachments WHERE message_id IS NULL AND created_at_ms<=$2 ORDER BY created_at_ms LIMIT $3)",
            now,
            now - DAY,
            batch,
        )
        .await?,
    );
    // Observation rows are latest per scope/source, and expire in <=1 hour.
    // Keep worker evidence until its original task has settled for 30 days.
    let query = format!(
        r#"DELETE FROM intelligence_observations WHERE scope_source_digest IN (
      SELECT o.scope_source_digest FROM intelligence_observations o
      WHERE (o.observation->>'received_at_ms')::bigint<$2 AND (o.observation->>'expires_at_ms')::bigint<$1
      AND (o.observation#>>'{{source,kind}}'='operator' OR EXISTS(SELECT 1 FROM execution_attempts e
        WHERE e.attempt_id::text=o.observation#>>'{{source,attempt_id}}' AND {ELIGIBLE}))
      ORDER BY o.scope_source_digest LIMIT $3)"#
    );
    report.insert(
        "intelligence_observations",
        statement(pool, &query, now, now - 30 * DAY, batch).await?,
    );
    // node_id is the PK: every row is already the latest snapshot for that node.
    report.insert("intelligence_node_snapshots", 0);
    Ok(report)
}

async fn packets(pool: &PgPool, now: i64, batch: i64, report: &mut Report) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL aq.row_retention='on'")
        .execute(&mut *tx)
        .await?;
    // Lock nodes before their accounting rows, as ingestion does. Candidates
    // whose attempts are not all fenced/eligible or still have events stay put.
    let query = format!(
        r#"SELECT p.node_id,p.message_id,p.charged_bytes FROM observation_packet_retention p
      WHERE p.recorded_at_ms<$2 AND cardinality(p.attempt_ids)>0
      AND NOT EXISTS(SELECT 1 FROM unnest(p.attempt_ids) a(id) WHERE NOT EXISTS(
        SELECT 1 FROM execution_attempts e JOIN retention_closed_attempts c USING(attempt_id) WHERE e.attempt_id=a.id AND {ELIGIBLE}))
      AND NOT EXISTS(SELECT 1 FROM observation_events o WHERE o.node_id=p.node_id AND o.sequence BETWEEN p.first_sequence AND p.last_sequence)
      ORDER BY p.node_id,p.message_id LIMIT $3"#
    );
    let rows = sqlx::query(&query)
        .bind(now)
        .bind(now - 14 * DAY)
        .bind(batch)
        .fetch_all(&mut *tx)
        .await?;
    let mut deleted = 0;
    for row in &rows {
        let node: Uuid = row.get("node_id");
        let message: Uuid = row.get("message_id");
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(node)
            .execute(&mut *tx)
            .await?;
        let charge: Option<i64> = sqlx::query_scalar("DELETE FROM observation_packet_retention WHERE node_id=$1 AND message_id=$2 RETURNING charged_bytes")
            .bind(node)
            .bind(message)
            .fetch_optional(&mut *tx)
            .await?;
        // Another collector may have removed a candidate while this transaction
        // waited for the node lock. Return its quota charge only once.
        let Some(charge) = charge else { continue };
        sqlx::query("DELETE FROM observation_packets WHERE node_id=$1 AND message_id=$2")
            .bind(node)
            .bind(message)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE observation_nodes SET retained_bytes=retained_bytes-$2 WHERE node_id=$1",
        )
        .bind(node)
        .bind(charge)
        .execute(&mut *tx)
        .await?;
        deleted += 1;
    }
    tx.commit().await?;
    report.insert("observation_packets", deleted);
    report.insert("observation_packet_retention", deleted);
    Ok(())
}

async fn outboxes(pool: &PgPool, now: i64, batch: i64, report: &mut Report) -> anyhow::Result<()> {
    for (table, done) in [
        ("agent_graph_outbox", "delivered AND notification_observed"),
        (
            "playbook_outbox",
            "status='engine_accepted' AND fanout_complete",
        ),
        ("review_owner_outbox", "acknowledged"),
        ("terminal_outbox", "acknowledged"),
    ] {
        // Age begins when ALL consumers have completed, never at enqueue time.
        statement(pool,&format!("UPDATE {table} SET retention_completed_at_ms=$1 WHERE ctid IN (SELECT ctid FROM {table} WHERE ({done}) AND retention_completed_at_ms IS NULL AND $2<$1 LIMIT $3)"),now,now-7*DAY,batch).await?;
        let authority=match table {
            "agent_graph_outbox"=>format!("EXISTS(SELECT 1 FROM agent_graph_operations o JOIN execution_attempts e USING(attempt_id) WHERE o.operation_id=b.operation_id AND {ELIGIBLE})"),
            "review_owner_outbox"=>"EXISTS(SELECT 1 FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) JOIN retention_tasks t ON t.task_id=s.task_id AND t.workspace_id=s.workspace_id WHERE d.decision_id=b.decision_id AND d.state<>'pending' AND t.checked_at_ms=$1 AND t.settled_since_ms<$2 AND NOT EXISTS(SELECT 1 FROM execution_attempts e WHERE e.task_id=t.task_id AND (e.holds_task OR COALESCE((SELECT h.state->>'status' FROM profiled_authority_hot h WHERE h.attempt_id=e.attempt_id),e.state->>'status','unknown') NOT IN ('finished','declined','never_granted'))))".to_owned(),
            "terminal_outbox"=>format!("EXISTS(SELECT 1 FROM terminal_sessions s JOIN execution_attempts e USING(attempt_id) WHERE s.terminal_id=b.terminal_id AND {ELIGIBLE})"),
            // All fanout is complete; immutable admissions/effects and engine
            // receipts retain operation idempotency independently of this queue.
            _=>"TRUE".into(),
        };
        let query = format!(
            "DELETE FROM {table} WHERE ctid IN (SELECT b.ctid FROM {table} b WHERE ({done}) AND retention_completed_at_ms<$2 AND {authority} ORDER BY retention_completed_at_ms LIMIT $3)"
        );
        if table == "playbook_outbox" {
            // Return namespace row capacity as well as deleting the queue body.
            let query = format!(
                "WITH removed AS ({query} RETURNING namespace_id,generation,octet_length(ciphertext)+4096 AS charge) UPDATE playbook_namespaces n SET outbox_count=outbox_count-r.n,retained_bytes=retained_bytes-r.charge FROM (SELECT namespace_id,generation,count(*) n,sum(charge) charge FROM removed GROUP BY namespace_id,generation) r WHERE n.namespace_id=r.namespace_id AND n.generation=r.generation AND $1::bigint>0 RETURNING r.n"
            );
            let rows: Vec<i64> = sqlx::query_scalar(&query)
                .bind(now)
                .bind(now - 7 * DAY)
                .bind(batch)
                .fetch_all(pool)
                .await?;
            report.insert(table, rows.iter().sum::<i64>() as u64);
        } else {
            report.insert(
                table,
                statement(pool, &query, now, now - 7 * DAY, batch).await?,
            );
        }
    }
    Ok(())
}

async fn terminals(pool: &PgPool, now: i64, batch: i64, report: &mut Report) -> anyhow::Result<()> {
    // Session end is conservatively the observation of all task attempts being
    // released. Do not destroy pending input, active subscriptions or receipts
    // while their explicit deadline/idempotency period remains open.
    for (table, extra) in [
        ("terminal_subscriptions", "b.expires_at_ms<$2"),
        ("terminal_control_acl", "b.expires_at_ms<$2"),
        (
            "terminal_input_receipts",
            "b.deadline_ms<$2 AND b.updated_at_ms<$2 AND b.disposition IN ('applied','delivery_unknown','expired','rejected')",
        ),
        ("terminal_operations", "b.created_at_ms<$2"),
    ] {
        let query = format!(
            r#"DELETE FROM {table} WHERE ctid IN (SELECT b.ctid FROM {table} b
          JOIN terminal_sessions s USING(terminal_id) JOIN execution_attempts e USING(attempt_id)
          WHERE {ELIGIBLE} AND {extra} ORDER BY b.terminal_id LIMIT $3)"#
        );
        report.insert(
            table,
            statement(pool, &query, now, now - 7 * DAY, batch).await?,
        );
    }
    let query = format!(
        r#"DELETE FROM terminal_sessions WHERE terminal_id IN (SELECT s.terminal_id FROM terminal_sessions s
      JOIN execution_attempts e USING(attempt_id) WHERE {ELIGIBLE} AND s.observed_at_ms<$2
      AND NOT EXISTS(SELECT 1 FROM terminal_operations x WHERE x.terminal_id=s.terminal_id)
      AND NOT EXISTS(SELECT 1 FROM terminal_input_receipts x WHERE x.terminal_id=s.terminal_id)
      AND NOT EXISTS(SELECT 1 FROM terminal_subscriptions x WHERE x.terminal_id=s.terminal_id)
      AND NOT EXISTS(SELECT 1 FROM terminal_control_acl x WHERE x.terminal_id=s.terminal_id)
      AND NOT EXISTS(SELECT 1 FROM terminal_outbox x WHERE x.terminal_id=s.terminal_id)
      ORDER BY s.terminal_id LIMIT $3)"#
    );
    report.insert(
        "terminal_sessions",
        statement(pool, &query, now, now - 7 * DAY, batch).await?,
    );
    // Inspection ACL can be shared: remove only when no terminal still uses it.
    let query = r#"DELETE FROM session_inspection_acl WHERE ctid IN(SELECT b.ctid FROM session_inspection_acl b
      WHERE expires_at_ms<$2 AND $2<$1 AND NOT EXISTS(SELECT 1 FROM terminal_sessions s WHERE s.identity->'session'=b.session_identity)
      ORDER BY expires_at_ms LIMIT $3)"#;
    report.insert(
        "session_inspection_acl",
        statement(pool, query, now, now - 7 * DAY, batch).await?,
    );
    Ok(())
}

async fn mail(pool: &PgPool, now: i64, batch: i64, report: &mut Report) -> anyhow::Result<()> {
    // There is no message TTL in the current schema. Session mail expires only
    // after its exact attempt has released; task mail needs a read receipt.
    // Global supervisor originals are authoritative append-only sources and
    // are deliberately excluded (their source guard also rejects deletion).
    let query = r#"SELECT m.message_id FROM mailbox_messages m JOIN retention_tasks t
      ON t.task_id::text=m.recipient->>'task_id' AND t.workspace_id::text=m.recipient->>'workspace_id'
      WHERE m.recipient->>'kind' IN ('task','session') AND m.created_at_ms<$2
      AND t.checked_at_ms=$1 AND t.settled_since_ms<$2
      AND NOT EXISTS(SELECT 1 FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id)
        WHERE e.task_id=t.task_id AND (e.holds_task OR COALESCE(h.state->>'status',e.state->>'status','unknown') NOT IN ('finished','declined','never_granted')))
      AND (m.retention_ready_at_ms IS NOT NULL OR EXISTS(SELECT 1 FROM mailbox_receipts r WHERE r.message_id=m.message_id AND r.disposition='read' AND r.acknowledged_at_ms<$2)
        OR (m.recipient->>'kind'='session' AND EXISTS(SELECT 1 FROM execution_attempts e WHERE e.attempt_id::text=m.recipient->>'attempt_id' AND NOT e.holds_task)))
      AND NOT EXISTS(SELECT 1 FROM central_supervisor_mail_members c WHERE c.message_id=m.message_id)
      AND NOT EXISTS(SELECT 1 FROM central_supervisor_replies r WHERE r.reply_message_id=m.message_id)
      ORDER BY m.created_at_ms,m.message_id LIMIT $3"#;
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut *tx)
        .await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(query)
        .bind(now)
        .bind(now - 30 * DAY)
        .bind(batch)
        .fetch_all(&mut *tx)
        .await?;
    // Retain eligibility after the read receipt is removed in an earlier batch.
    sqlx::query("UPDATE mailbox_messages SET retention_ready_at_ms=COALESCE(retention_ready_at_ms,$2) WHERE message_id=ANY($1)")
        .bind(&ids).bind(now).execute(&mut *tx).await?;
    // Each child also has its own batch cap. A parent waits for another tick
    // when it has more than one batch of historical attempts/deliveries.
    for table in ["mailbox_receipts", "mailbox_deliveries"] {
        let n=sqlx::query(&format!("DELETE FROM {table} WHERE ctid IN (SELECT ctid FROM {table} WHERE message_id=ANY($1) LIMIT $2)"))
            .bind(&ids).bind(batch).execute(&mut *tx).await?.rows_affected();
        report.insert(table, n);
    }
    let n=sqlx::query("DELETE FROM mailbox_messages m WHERE message_id=ANY($1) AND NOT EXISTS(SELECT 1 FROM mailbox_receipts r WHERE r.message_id=m.message_id) AND NOT EXISTS(SELECT 1 FROM mailbox_deliveries d WHERE d.message_id=m.message_id)")
        .bind(&ids).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    report.insert("mailbox_messages", n);
    Ok(())
}

#[async_trait::async_trait]
impl<T: TaskState + ?Sized> TaskState for Option<std::sync::Arc<T>> {
    async fn completed(&self, workspace: Uuid, task: Uuid) -> anyhow::Result<bool> {
        match self {
            Some(rt) => rt.completed(workspace, task).await,
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Always(bool);
    #[async_trait::async_trait]
    impl TaskState for Always {
        async fn completed(&self, _: Uuid, _: Uuid) -> anyhow::Result<bool> {
            Ok(self.0)
        }
    }

    #[tokio::test]
    async fn option_arc_of_task_state_delegates_and_none_is_incomplete() {
        let (w, t) = (Uuid::nil(), Uuid::nil());
        assert!(Some(Arc::new(Always(true))).completed(w, t).await.unwrap());
        assert!(!None::<Arc<Always>>.completed(w, t).await.unwrap());
    }
}
