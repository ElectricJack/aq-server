-- Owner decision 2026-11-18: an explicit owner retry always releases a task a
-- limit holds (attempts, lineage time, sandbox time, cost or never-granted
-- offers used up). The override is recorded here, once per owner operation,
-- with the hold it overrode and the owner's reason. Every budget of the task
-- starts again: attempts offered and never-granted offers closed up to
-- `recorded_at_ms` are not charged (aq-profiled-state
-- `replacement_sql::PRIORS_149`, aq-machine-control `attempts::COUNTS_149`),
-- and only spend above `spent_microunits` counts. Rows are never changed.
-- Additive: a new table and one more allowed `task_holds.cleared_by` value;
-- no row is touched, so an older server keeps running on the migrated schema.
SET LOCAL lock_timeout = '5s';
CREATE TABLE task_limit_overrides (
    operation_id UUID PRIMARY KEY,
    task_id UUID NOT NULL,
    -- The task's latest attempt when the owner overrode the hold.
    attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
    hold_code TEXT NOT NULL CHECK (hold_code ~ '^[a-z][a-z0-9_]{0,95}$'),
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 1024),
    actor TEXT NOT NULL CHECK (length(actor) BETWEEN 1 AND 320),
    recorded_at_ms BIGINT NOT NULL CHECK (recorded_at_ms > 0),
    -- The lineage's accounted spend at the override (NULL when unknown). Only
    -- spend above it counts against the cost budget afterwards.
    spent_microunits BIGINT CHECK (spent_microunits >= 0)
);
CREATE INDEX task_limit_overrides_task ON task_limit_overrides(task_id, recorded_at_ms);
CREATE FUNCTION task_limit_overrides_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 RAISE EXCEPTION 'task limit overrides are immutable';
END $$;
CREATE TRIGGER task_limit_overrides_no_change BEFORE UPDATE OR DELETE
  ON task_limit_overrides FOR EACH ROW EXECUTE FUNCTION task_limit_overrides_immutable();
ALTER TABLE task_holds DROP CONSTRAINT task_holds_cleared_by_check;
ALTER TABLE task_holds ADD CONSTRAINT task_holds_cleared_by_check
  CHECK (cleared_by IN ('new_attempt','claim_released','task_closed','owner_override'));
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='149' WHERE key='state_format_version';
