-- Epic 0, delivery by playbook (spec 0016 section 6.2): what the host records
-- for the catalog 2.3.0 commands `work.return` and `supervisor.notify`.
-- Additive: two nullable columns, a new table and two wider checks; no row is
-- changed, so an older server keeps running on the migrated schema.
SET LOCAL lock_timeout = '5s';

-- `work.return` passes on, for the task's next attempt, the fact value the
-- playbook gave (`detail`) and whether that attempt starts with current main
-- merged in (`update_from_main`). Only a return carries them.
ALTER TABLE delivery_decisions
    ADD COLUMN detail TEXT CHECK(detail IS NULL OR octet_length(detail) <= 1024),
    ADD COLUMN update_from_main BOOLEAN,
    ADD CHECK(decision = 'returned' OR (detail IS NULL AND update_from_main IS NULL)),
    ADD CHECK(decision <> 'returned' OR update_from_main IS NOT NULL);

-- `supervisor.notify` tells a project's supervisor once per identity: one row
-- per (project, digest of the identity), written in the same transaction as
-- the supervisor message it stands for.
CREATE TABLE delivery_notifications (
    project_id UUID NOT NULL,
    identity_digest TEXT NOT NULL CHECK(identity_digest ~ '^sha256:[0-9a-f]{64}$'),
    message_id UUID NOT NULL,
    notified_at_ms BIGINT NOT NULL CHECK(notified_at_ms > 0),
    PRIMARY KEY(project_id, identity_digest)
);

-- The intent rows of the four decision commands and their final outcomes.
-- The lists only grow, so an older server's rows stay valid.
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_command_name_check;
ALTER TABLE playbook_effects ADD CHECK(command_name IN ('message.send','decision.request','context.read','work.ensure','work.change','operation.cancel','findings.reconcile','judgment.request','check.request','candidate.prepare','candidate.publish','work.read','work.route','work.unroutable','judgment.assess','work.complete','work.return','work.hold','supervisor.notify'));
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_status_check;
ALTER TABLE playbook_effects ADD CHECK(status IN ('queued','refused','cancelled','accepted','pending','ensured','changed','conflict','observed','incomplete','expired','too_late','indeterminate','reconciled','prepared','published','routed','stale','recorded','completed','returned','held','sent','duplicate'));
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='144' WHERE key='state_format_version';
