-- Owner decision 2026-11-19: an owner retry may start a task fresh. A retry
-- reuses the replaced attempt's retained source, so it must run under the
-- project binding revision that attempt was offered under; when the binding
-- changed the offer path holds the task (`replacement_project_revision_changed`).
-- A fresh retry releases that attempt the way a replacement does (0052) and
-- records here, once per attempt of the task at that moment, that those
-- attempts are no longer the lineage's history: the next attempt is a first
-- attempt from the project's current pinned source under the current binding
-- (aq-profiled-state `replacement_sql::PRIORS_156`, aq-machine-control
-- `attempts::COUNTS_156`). Nothing is deleted: the attempts, their retained
-- results and the replacement release stay. Rows are never changed.
-- Additive: a new table; no row is touched, so an older server keeps running
-- on the migrated schema.
SET LOCAL lock_timeout = '5s';
CREATE TABLE task_fresh_starts (
    attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id),
    task_id UUID NOT NULL,
    operation_id UUID NOT NULL,
    -- The held attempt the owner released.
    released_attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
    hold_code TEXT NOT NULL CHECK (hold_code ~ '^[a-z][a-z0-9_]{0,95}$'),
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 1024),
    actor TEXT NOT NULL CHECK (length(actor) BETWEEN 1 AND 320),
    recorded_at_ms BIGINT NOT NULL CHECK (recorded_at_ms > 0),
    -- The project binding revision the fresh attempt starts under.
    project_revision BIGINT NOT NULL CHECK (project_revision > 0)
);
CREATE INDEX task_fresh_starts_task ON task_fresh_starts(task_id, recorded_at_ms);
CREATE FUNCTION task_fresh_starts_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 RAISE EXCEPTION 'task fresh starts are immutable';
END $$;
CREATE TRIGGER task_fresh_starts_no_change BEFORE UPDATE OR DELETE
  ON task_fresh_starts FOR EACH ROW EXECUTE FUNCTION task_fresh_starts_immutable();
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='156' WHERE key='state_format_version';
