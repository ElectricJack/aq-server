//! Rolling deletion of aq-server's finished playbook bridge rows
//! (architecture 0006: no lifetime caps; old records are deleted).
//!
//! `[playbooks.retention]`: each table keeps the newest `keep_finished`
//! finished rows of a namespace generation; one pass deletes at most
//! `delete_batch` older ones. Rows of work in progress never match: pending,
//! held or unknown rows, running operations, unanswered decisions, unexpired
//! scans, unsent notifications, open finding episodes. The aq-server drive quantum runs one
//! statement per quantum, in turn.
//!
//! Each statement binds `$1` namespace id, `$2` generation, `$3`
//! keep_finished, `$4` delete_batch and `$5` the server clock (ms).
use serde::Deserialize;

/// `[playbooks.retention]`; defaults keep 10,000 finished rows per table and
/// delete at most 500 per pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retention {
    pub keep_finished: u64,
    pub delete_batch: u64,
}
impl Default for Retention {
    fn default() -> Self {
        Self {
            keep_finished: 10_000,
            delete_batch: 500,
        }
    }
}
impl Retention {
    /// Both positive: keeping nothing would delete what idempotent replay
    /// reads, and a pass that deletes nothing never drains. No upper bound.
    pub fn valid(&self) -> bool {
        self.bounds().is_some()
    }
    /// `(keep_finished, delete_batch)` as SQL bigints.
    pub fn bounds(&self) -> Option<(i64, i64)> {
        let keep = i64::try_from(self.keep_finished).ok()?;
        let batch = i64::try_from(self.delete_batch).ok()?;
        (keep > 0 && batch > 0).then_some((keep, batch))
    }
}

/// `(table, statement)` in the order the drive takes them.
pub const TABLES: [(&str, &str); 9] = [
    // Settled events: accepted and fanned out. Producers dedupe on their own
    // source (graph and settlement rows are acknowledged with the engine
    // acceptance, decisions when copied, host-bridge events with their effect).
    (
        "playbook_outbox",
        "DELETE FROM playbook_outbox WHERE (namespace_id,generation,producer,event_id) IN (SELECT namespace_id,generation,producer,event_id FROM playbook_outbox WHERE namespace_id=$1 AND generation=$2 AND status='engine_accepted' AND fanout_complete ORDER BY sequence DESC OFFSET $3 LIMIT $4) AND (plugin_retain_until_ms IS NULL OR plugin_retain_until_ms<=$5) AND $5::bigint>0",
    ),
    // Accepted admissions, once their grant can no longer admit: a repeat of
    // a deleted request id is then refused (expired or revoked grant).
    (
        "playbook_admissions",
        "DELETE FROM playbook_admissions WHERE (namespace_id,generation,request_id) IN (SELECT a.namespace_id,a.generation,a.request_id FROM (SELECT namespace_id,generation,request_id,grant_id,grant_revision FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND status='engine_accepted' ORDER BY created_at_ms DESC,request_id DESC OFFSET $3) a JOIN playbook_grants g ON g.grant_id=a.grant_id WHERE g.revoked_at_ms IS NOT NULL OR g.expires_at_ms<=$5 OR g.revision<>a.grant_revision LIMIT $4)",
    ),
    // Recorded effects of finished runs (the run and its root are in the
    // engine's history, migration 0128): until then the engine may still ask
    // for the result (reconcile after a lost acknowledgement). Also never one
    // whose operation is still running, nor an accepted review decision
    // (handle and subject) not yet final and copied.
    (
        "playbook_effects",
        "DELETE FROM playbook_effects WHERE (namespace_id,generation,intent_id) IN (\
         SELECT e.namespace_id,e.generation,e.intent_id FROM playbook_effects e \
         WHERE e.namespace_id=$1 AND e.generation=$2 AND e.status<>'pending' \
         AND playbook_run_finished($1,$2,e.run_id) \
         AND NOT EXISTS(SELECT 1 FROM playbook_host_operations o WHERE o.namespace_id=e.namespace_id \
          AND o.generation=e.generation AND o.intent_id=e.intent_id AND o.status='running') \
         AND (e.status<>'accepted' OR e.command_name<>'decision.request' OR e.receipt->'subject' IS NULL \
          OR (EXISTS(SELECT 1 FROM human_decision_finality f WHERE f.decision_id=(e.receipt->>'handle')::uuid) \
          AND NOT EXISTS(SELECT 1 FROM review_owner_outbox r \
           WHERE r.decision_id=(e.receipt->>'handle')::uuid AND NOT r.acknowledged))) \
         ORDER BY e.created_at_ms DESC,e.intent_id DESC OFFSET $3 LIMIT $4) AND $5::bigint>0",
    ),
    (
        "playbook_cancellations",
        "DELETE FROM playbook_cancellations WHERE (namespace_id,generation,cancellation_key) IN (SELECT namespace_id,generation,cancellation_key FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND disposition IN ('confirmed','too_late','refused') ORDER BY created_at_ms DESC,cancellation_key DESC OFFSET $3 LIMIT $4) AND $5::bigint>0",
    ),
    (
        "playbook_host_operations",
        // A completed operation is evidence candidate.publish reads: kept
        // while the run that requested it is not finished.
        "DELETE FROM playbook_host_operations WHERE operation_id IN (SELECT o.operation_id FROM playbook_host_operations o WHERE o.namespace_id=$1 AND o.generation=$2 AND o.status<>'running' AND NOT EXISTS(SELECT 1 FROM playbook_effects e WHERE e.namespace_id=o.namespace_id AND e.generation=o.generation AND e.intent_id=o.intent_id AND NOT playbook_run_finished($1,$2,e.run_id)) ORDER BY o.completed_at_ms DESC NULLS LAST,o.operation_id DESC OFFSET $3 LIMIT $4) AND $5::bigint>0",
    ),
    // Health scans once reconciled or expired (an expired scan cannot be).
    (
        "playbook_health_observations",
        "DELETE FROM playbook_health_observations WHERE (namespace_id,generation,observation) IN (SELECT namespace_id,generation,observation FROM playbook_health_observations WHERE namespace_id=$1 AND generation=$2 AND (result IS NOT NULL OR expires_at_ms<=$5) ORDER BY expires_at_ms DESC,observation DESC OFFSET $3 LIMIT $4)",
    ),
    (
        "playbook_finding_notifications",
        "DELETE FROM playbook_finding_notifications WHERE (namespace_id,generation,notification_id) IN (SELECT namespace_id,generation,notification_id FROM playbook_finding_notifications WHERE namespace_id=$1 AND generation=$2 AND submitted ORDER BY sequence DESC OFFSET $3 LIMIT $4) AND $5::bigint>0",
    ),
    (
        "playbook_plugin_notifications",
        "DELETE FROM playbook_plugin_notifications WHERE identity IN (SELECT identity FROM playbook_plugin_notifications WHERE namespace_id=$1 AND generation=$2 AND submitted AND created_at_ms<($5::bigint-86400000) ORDER BY created_at_ms DESC OFFSET $3 LIMIT $4)",
    ),
    // Cleared finding episodes (the finding is gone). An open episode is
    // work in progress and never matches. A finding that comes back after
    // its cleared episode was deleted starts a new episode at revision 1, so
    // none is deleted while an unexpired scan of its partition is still to
    // be reconciled: that scan's baseline holds the old revisions.
    (
        "playbook_finding_episodes",
        "DELETE FROM playbook_finding_episodes WHERE (namespace_id,generation,partition,finding_key) IN (SELECT namespace_id,generation,partition,finding_key FROM playbook_finding_episodes WHERE namespace_id=$1 AND generation=$2 AND NOT present ORDER BY last_notified_ms DESC,partition DESC,finding_key DESC OFFSET $3 LIMIT $4) AND NOT EXISTS(SELECT 1 FROM playbook_health_observations s WHERE s.namespace_id=$1 AND s.generation=$2 AND s.partition=playbook_finding_episodes.partition AND s.result IS NULL AND s.expires_at_ms>$5)",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retention_defaults_to_10000_and_500_and_refuses_zero() {
        let parsed: Retention = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, Retention::default());
        assert_eq!(parsed.bounds(), Some((10_000, 500)));
        let set: Retention =
            serde_json::from_str(r#"{"keep_finished":3,"delete_batch":1}"#).unwrap();
        assert_eq!(set.bounds(), Some((3, 1)));
        // No upper bound below what SQL can bind.
        let large = Retention {
            keep_finished: i64::MAX as u64,
            delete_batch: i64::MAX as u64,
        };
        assert!(large.valid());
        for (keep, batch) in [(0, 500), (10_000, 0), (u64::MAX, 1), (1, u64::MAX)] {
            let r = Retention {
                keep_finished: keep,
                delete_batch: batch,
            };
            assert!(!r.valid(), "{keep} {batch}");
        }
        assert!(serde_json::from_str::<Retention>(r#"{"keep":1}"#).is_err());
    }
    #[test]
    fn every_statement_binds_the_same_five_parameters_and_never_touches_work_in_progress() {
        for (table, sql) in TABLES {
            assert!(sql.starts_with(&format!("DELETE FROM {table} ")), "{table}");
            for p in ["$1", "$2", "$3", "$4", "$5"] {
                assert!(sql.contains(p), "{table} {p}");
            }
            assert!(!sql.contains("$6"), "{table}");
            for live in ["'pending')", "'unknown'"] {
                assert!(!sql.contains(live), "{table} {live}");
            }
        }
    }
}
