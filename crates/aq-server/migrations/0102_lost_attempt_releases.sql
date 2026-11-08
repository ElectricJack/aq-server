-- Swarm phase 1, D2 ruling C: a restarted node's release of an attempt whose
-- host was lost (its guardian cannot prove the stop). The outside proof that
-- every process of the host is dead is kept as sent: boot_changed (a reboot,
-- not charged to the lineage budgets) or cgroup_teardown (a crash, charged).
-- The attempt is failed and its task retried; nothing of it is salvaged.
CREATE TABLE lost_attempt_releases (
  attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id),
  node_id UUID NOT NULL REFERENCES nodes(id),
  proof_kind TEXT NOT NULL CHECK (proof_kind IN ('boot_changed','cgroup_teardown')),
  proof JSONB NOT NULL CHECK (octet_length(proof::text) <= 4096),
  reason TEXT NOT NULL CHECK (
    (proof_kind = 'boot_changed' AND reason = 'lost_after_reboot')
    OR (proof_kind = 'cgroup_teardown' AND reason = 'lost_after_crash')),
  charged BOOLEAN NOT NULL CHECK (charged = (proof_kind = 'cgroup_teardown')),
  receipt_digest TEXT NOT NULL CHECK (receipt_digest ~ '^[0-9a-f]{64}$'),
  recorded_at TIMESTAMPTZ NOT NULL,
  -- Why B4 still holds the task after the release (for example an exhausted
  -- budget); NULL once it is released for the retry. The node stays busy.
  held_reason TEXT CHECK (held_reason IS NULL OR length(held_reason) BETWEEN 1 AND 256)
);
CREATE INDEX lost_attempt_releases_node ON lost_attempt_releases(node_id);
-- A started attempt's hot stop becomes 'stopped' only with its original stopped
-- receipt (0021), or now with its lost-attempt release record: the stop names
-- aq.lost-attempt-release/<attempt> and that record's digest.
CREATE OR REPLACE FUNCTION profiled_stopped_projection_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.state#>>'{stop,kind}'='stopped' AND OLD.state#>>'{stop,kind}'<>'stopped'
 AND EXISTS(SELECT 1 FROM profiled_started_receipts WHERE attempt_id=NEW.attempt_id)
 AND NOT EXISTS(SELECT 1 FROM profiled_stopped_receipts r JOIN profiled_owner_journals j USING(attempt_id)
  WHERE r.attempt_id=NEW.attempt_id AND r.before_record_revision=OLD.record_revision AND j.phase='stopped'
  AND NEW.state->'stop'=jsonb_build_object('kind','stopped','receipt',jsonb_build_object('id','aq.native-stopped/'||NEW.attempt_id::TEXT,'version','1','digest',encode(sha256(r.receipt_bytes),'hex'))))
 AND NOT EXISTS(SELECT 1 FROM lost_attempt_releases l WHERE l.attempt_id=NEW.attempt_id
  AND NEW.state->'stop'=jsonb_build_object('kind','stopped','receipt',jsonb_build_object('id','aq.lost-attempt-release/'||NEW.attempt_id::TEXT,'version','1','digest',l.receipt_digest)))
 THEN RAISE EXCEPTION 'native stopped projection requires original stopped receipt'; END IF;
 RETURN NEW;
END $$;
UPDATE meta SET value='102' WHERE key='state_format_version';
