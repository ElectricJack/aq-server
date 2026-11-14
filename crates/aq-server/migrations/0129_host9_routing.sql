-- Playbook authoring B2 and B5 (spec 0013 §6, §7).
-- Catalog profile 9 (AQ host catalog 2.2.0): profile 8 plus work.read,
-- work.route, work.unroutable and judgment.assess. Existing profiles,
-- grants, catalogs and rows are unchanged; this only admits new rows.
-- route_needed_emissions records why a task is not asked about again: the
-- route was refused `not_offered` or the playbook said `work.unroutable`
-- (with its reasons), under which active routing artifact and which
-- Settings offers. A new task version, artifact or (for not_offered)
-- offers asks again. workspace_id and project_id say where the task lives,
-- so a routing playbook can read a task of any project on the router (R1).
-- Placement records a ready task that waits for a route (wanted_version,
-- wanted_at_ms); the playbook owner sends aq.work.route_needed from these
-- rows, where task_version and emitted_at_ms are the version last asked
-- about and when (0: never), and checked_at_ms when a row was last looked at
-- without asking.
SET LOCAL lock_timeout = '5s';
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3,4,5,6,7,8,9));
-- The four catalog 2.2.0 commands and their outcomes on the bridge tables.
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_command_name_check;
ALTER TABLE playbook_effects ADD CHECK(command_name IN ('message.send','decision.request','context.read','work.ensure','work.change','operation.cancel','findings.reconcile','judgment.request','check.request','candidate.prepare','candidate.publish','work.read','work.route','work.unroutable','judgment.assess'));
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_status_check;
ALTER TABLE playbook_effects ADD CHECK(status IN ('queued','refused','cancelled','accepted','pending','ensured','changed','conflict','observed','incomplete','expired','too_late','indeterminate','reconciled','prepared','published','routed','stale','recorded'));
ALTER TABLE playbook_host_operations DROP CONSTRAINT playbook_host_operations_command_name_check;
ALTER TABLE playbook_host_operations ADD CHECK (command_name IN ('judgment.request','check.request','work.ensure','decision.request','judgment.assess'));
ALTER TABLE route_needed_emissions
 ADD COLUMN outcome TEXT CHECK(outcome IN ('not_offered','unroutable')),
 ADD COLUMN reasons JSONB CHECK(reasons IS NULL OR jsonb_typeof(reasons) = 'array'),
 ADD COLUMN artifact_digest TEXT,
 ADD COLUMN options_digest TEXT,
 ADD COLUMN workspace_id UUID,
 ADD COLUMN project_id UUID,
 ADD COLUMN wanted_version BIGINT,
 ADD COLUMN wanted_at_ms BIGINT,
 ADD COLUMN checked_at_ms BIGINT NOT NULL DEFAULT 0;
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='129' WHERE key='state_format_version';
