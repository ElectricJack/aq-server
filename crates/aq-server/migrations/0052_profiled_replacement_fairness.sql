-- B4: failed-attempt replacement release and fair multi-project scheduling.
-- A replacement release frees the task claim of a failed predecessor whose
-- stop is proven, whose source is retained and whose effects are quiescent.
-- It is never a Graph completion; accepted attempts can never be released here.
CREATE TABLE profiled_replacement_releases (
 attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id),
 task_id UUID NOT NULL,
 reason TEXT NOT NULL CHECK(reason='failed_attempt_replacement'),
 stop_receipt_digest TEXT NOT NULL CHECK(stop_receipt_digest ~ '^[0-9a-f]{64}$'),
 retained_source_receipt_digest TEXT NOT NULL CHECK(retained_source_receipt_digest ~ '^[0-9a-f]{64}$'),
 effect_reconciliation_receipt_digest TEXT NOT NULL CHECK(effect_reconciliation_receipt_digest ~ '^[0-9a-f]{64}$'),
 attempts_started INT NOT NULL CHECK(attempts_started>0),
 recorded_at_ms BIGINT NOT NULL CHECK(recorded_at_ms>0)
);
CREATE FUNCTION profiled_replacement_release_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE a execution_attempts%ROWTYPE; h profiled_authority_hot%ROWTYPE;
BEGIN
 IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'immutable replacement release' USING ERRCODE='23514'; END IF;
 SELECT * INTO a FROM execution_attempts WHERE attempt_id=NEW.attempt_id;
 SELECT * INTO h FROM profiled_authority_hot WHERE attempt_id=NEW.attempt_id;
 IF a.attempt_id IS NULL OR h.attempt_id IS NULL OR a.kind<>'profiled_v1' OR NOT a.holds_task OR a.task_id<>NEW.task_id
    OR COALESCE(h.state#>>'{stop,kind}','') NOT IN ('stopped','not_started')
    OR COALESCE(h.state#>>'{effects,kind}','')<>'quiescent'
    OR COALESCE(h.state#>>'{retention,kind}','')<>'retained'
    OR NEW.stop_receipt_digest IS DISTINCT FROM h.state#>>'{stop,receipt,digest}'
    OR NEW.retained_source_receipt_digest IS DISTINCT FROM h.state#>>'{retention,receipt,digest}'
    OR NEW.effect_reconciliation_receipt_digest IS DISTINCT FROM h.state#>>'{effects,receipt,digest}'
    OR h.state->>'status' NOT IN ('finished','declined','cancelling','unknown')
    OR EXISTS(SELECT 1 FROM profiled_settlement_evaluations e WHERE e.attempt_id=NEW.attempt_id AND e.disposition IN ('accepted','needs_reconciliation','stale'))
    OR EXISTS(SELECT 1 FROM profiled_task_completions c WHERE c.attempt_id=NEW.attempt_id)
 THEN RAISE EXCEPTION 'replacement release requires stopped, retained, quiescent, unaccepted held attempt' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_replacement_release_guard BEFORE INSERT OR UPDATE OR DELETE ON profiled_replacement_releases
 FOR EACH ROW EXECUTE FUNCTION profiled_replacement_release_guard();
CREATE OR REPLACE FUNCTION profiled_historical_snapshot_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.kind='profiled_v1' AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
    AND (NEW.state IS DISTINCT FROM OLD.state OR NEW.execution_deadline_ms IS DISTINCT FROM OLD.execution_deadline_ms)
 THEN RAISE EXCEPTION 'hot authority owns current profiled state' USING ERRCODE='23514'; END IF;
 IF OLD.kind='profiled_v1' AND NOT OLD.holds_task AND NEW.holds_task
    AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
 THEN RAISE EXCEPTION 'released profiled claim cannot be reacquired' USING ERRCODE='23514'; END IF;
 IF OLD.kind='profiled_v1' AND OLD.holds_task AND NOT NEW.holds_task
    AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_verified_releases WHERE attempt_id=OLD.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_replacement_releases WHERE attempt_id=OLD.attempt_id)
    AND NOT profiled_never_granted_valid(OLD.attempt_id)
 THEN RAISE EXCEPTION 'actual owner-verified release record required' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
CREATE OR REPLACE FUNCTION settlement_claim_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.kind='profiled_v1' AND OLD.holds_task AND NOT NEW.holds_task AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
 AND NOT profiled_never_granted_valid(OLD.attempt_id)
 AND NOT EXISTS(SELECT 1 FROM profiled_replacement_releases WHERE attempt_id=OLD.attempt_id)
 AND NOT EXISTS(SELECT 1 FROM profiled_task_completions c JOIN profiled_settlement_evaluations e ON e.operation_id=c.evaluation_operation_id JOIN profiled_verified_releases r ON r.attempt_id=c.attempt_id AND r.release_id=c.release_id WHERE c.attempt_id=OLD.attempt_id AND c.state='confirmed' AND e.disposition='accepted' AND e.operation_id=r.settlement_operation_id AND c.receipt IS NOT NULL AND c.outbox IS NOT NULL)
 THEN RAISE EXCEPTION 'claim remains held until actual Graph completion receipt'; END IF;
 RETURN NEW;
END $$;
-- Fair share: last service per project; ordering itself is server code.
CREATE TABLE profiled_project_schedule (
 project_id UUID PRIMARY KEY,
 last_served_at_ms BIGINT NOT NULL CHECK(last_served_at_ms>0),
 served_count BIGINT NOT NULL DEFAULT 0 CHECK(served_count>=0)
);
UPDATE meta SET value='52' WHERE key='state_format_version';
