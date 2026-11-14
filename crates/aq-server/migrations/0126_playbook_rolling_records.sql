-- Playbook authoring B1b (architecture 0006): the playbook bridge keeps no
-- lifetime counters. playbook_namespaces counted grants (1,024), admissions
-- (4,096), effects and cancellations (8,192 each), outbox rows (16,384) and
-- retained bytes (64 MiB) and refused every operation at a cap, for good.
-- Dropping the columns drops their CHECKs. Finished rows are deleted on a
-- rolling basis instead ([playbooks.retention], the drive quantum); these
-- indexes serve its newest-first scans.
-- A lock that is not granted in 5 s fails this migration (55P03) instead of
-- hanging startup: the whole migration transaction rolls back and the server
-- exits, to retry.
SET LOCAL lock_timeout = '5s';
ALTER TABLE playbook_namespaces
 DROP COLUMN grant_count,
 DROP COLUMN admission_count,
 DROP COLUMN effect_count,
 DROP COLUMN cancellation_count,
 DROP COLUMN outbox_count,
 DROP COLUMN retained_bytes;
CREATE INDEX playbook_effects_age ON playbook_effects(namespace_id,generation,created_at_ms);
CREATE INDEX playbook_cancellations_age ON playbook_cancellations(namespace_id,generation,created_at_ms);
CREATE INDEX playbook_host_operations_completed ON playbook_host_operations(namespace_id,generation,completed_at_ms)
 WHERE status<>'running';
CREATE INDEX playbook_health_observations_expiry ON playbook_health_observations(namespace_id,generation,expires_at_ms);
CREATE INDEX playbook_finding_notifications_submitted ON playbook_finding_notifications(namespace_id,generation,sequence)
 WHERE submitted;
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='126' WHERE key='state_format_version';
