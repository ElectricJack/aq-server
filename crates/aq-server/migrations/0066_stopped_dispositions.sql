-- An owner may terminally abandon a proven stopped, unsettled task without
-- inventing source retention, effect reconciliation or Graph success.
CREATE TABLE profiled_stopped_dispositions (
 attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id),
 task_id UUID NOT NULL UNIQUE,
 operation_id UUID NOT NULL UNIQUE,
 actor_id UUID NOT NULL REFERENCES users(id),
 receipt JSONB NOT NULL CHECK(octet_length(receipt::TEXT)<=8192),
 CHECK(receipt->>'disposition'='abandoned'),
 CHECK(receipt->>'attempt_id'=attempt_id::TEXT),
 CHECK(receipt->>'task_id'=task_id::TEXT),
 CHECK(receipt->>'operation_id'=operation_id::TEXT),
 CHECK(receipt->>'actor_id'=actor_id::TEXT),
 CHECK(length(btrim(receipt->>'reason')) BETWEEN 1 AND 4096),
 CHECK((receipt->>'recorded_at_ms')::BIGINT>0)
);
CREATE FUNCTION profiled_stopped_disposition_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'immutable stopped disposition'; END IF;
 IF current_setting('aq.stopped_disposition_owner',true) IS DISTINCT FROM '1'
 OR NOT EXISTS(SELECT 1 FROM users WHERE id=NEW.actor_id AND role='owner')
 OR NOT EXISTS(SELECT 1 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id)
   JOIN profiled_stopped_receipts s USING(attempt_id)
   WHERE a.attempt_id=NEW.attempt_id AND a.task_id=NEW.task_id AND a.kind='profiled_v1' AND a.holds_task
   AND h.state->>'status'='finished' AND h.state#>>'{stop,kind}'='stopped'
   AND h.state#>>'{stop,receipt,digest}'=NEW.receipt->>'stop_receipt_digest'
   AND encode(sha256(s.receipt_bytes),'hex')=NEW.receipt->>'stop_receipt_digest')
 OR EXISTS(SELECT 1 FROM profiled_settlement_evaluations WHERE attempt_id=NEW.attempt_id AND disposition='accepted')
 OR EXISTS(SELECT 1 FROM profiled_task_completions WHERE attempt_id=NEW.attempt_id)
 OR EXISTS(SELECT 1 FROM profiled_auto_settlements WHERE attempt_id=NEW.attempt_id AND disposition IN ('prepared','possible','confirmed'))
 THEN RAISE EXCEPTION 'owner disposition requires stopped unsettled held attempt' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_stopped_disposition_guard BEFORE INSERT OR UPDATE OR DELETE ON profiled_stopped_dispositions
 FOR EACH ROW EXECUTE FUNCTION profiled_stopped_disposition_guard();
-- Receipt and released claim must survive (or roll back) together.
CREATE FUNCTION profiled_stopped_disposition_committed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF EXISTS(SELECT 1 FROM execution_attempts WHERE attempt_id=NEW.attempt_id AND holds_task)
 THEN RAISE EXCEPTION 'stopped disposition must atomically release claim'; END IF;
 RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER profiled_stopped_disposition_committed AFTER INSERT ON profiled_stopped_dispositions
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION profiled_stopped_disposition_committed();
CREATE OR REPLACE FUNCTION settlement_claim_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.kind='profiled_v1' AND OLD.holds_task AND NOT NEW.holds_task AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
 AND NOT profiled_never_granted_valid(OLD.attempt_id)
 AND NOT EXISTS(SELECT 1 FROM profiled_replacement_releases WHERE attempt_id=OLD.attempt_id)
 AND NOT EXISTS(SELECT 1 FROM profiled_stopped_dispositions WHERE attempt_id=OLD.attempt_id)
 AND NOT EXISTS(SELECT 1 FROM profiled_task_completions c JOIN profiled_settlement_evaluations e ON e.operation_id=c.evaluation_operation_id JOIN profiled_verified_releases r ON r.attempt_id=c.attempt_id AND r.release_id=c.release_id WHERE c.attempt_id=OLD.attempt_id AND c.state='confirmed' AND e.disposition='accepted' AND e.operation_id=r.settlement_operation_id AND c.receipt IS NOT NULL AND c.outbox IS NOT NULL)
 AND NOT EXISTS(SELECT 1 FROM profiled_auto_settlements a WHERE a.attempt_id=OLD.attempt_id AND a.disposition='confirmed' AND a.receipt IS NOT NULL)
 THEN RAISE EXCEPTION 'claim remains held until actual Graph completion receipt'; END IF;
 RETURN NEW;
END $$;
-- A terminal disposition is also an owner-verified claim release record.
CREATE OR REPLACE FUNCTION profiled_historical_snapshot_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.kind='profiled_v1' AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
    AND (NEW.state IS DISTINCT FROM OLD.state OR NEW.execution_deadline_ms IS DISTINCT FROM OLD.execution_deadline_ms)
    AND NOT (
      NOT NEW.holds_task
      AND EXISTS(SELECT 1 FROM profiled_authority_hot h
        JOIN profiled_task_completions c USING(attempt_id)
        JOIN profiled_verified_releases r USING(attempt_id,release_id)
        WHERE h.attempt_id=OLD.attempt_id AND c.state='confirmed'
          AND c.receipt IS NOT NULL AND c.outbox IS NOT NULL
          AND h.state->>'status'='finished' AND h.state#>>'{stop,kind}'='stopped'
          AND h.state#>>'{retention,kind}'='retained' AND h.state#>>'{effects,kind}'='quiescent'
          AND NEW.execution_deadline_ms IS NOT DISTINCT FROM (h.state->>'execution_deadline_ms')::bigint
          AND NEW.state IS NOT DISTINCT FROM OLD.state || (h.state-ARRAY['owner','preparation_operation_id']))
    )
 THEN RAISE EXCEPTION 'hot authority owns current profiled state' USING ERRCODE='23514'; END IF;
 IF OLD.kind='profiled_v1' AND NOT OLD.holds_task AND NEW.holds_task
    AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
 THEN RAISE EXCEPTION 'released profiled claim cannot be reacquired' USING ERRCODE='23514'; END IF;
 IF OLD.kind='profiled_v1' AND OLD.holds_task AND NOT NEW.holds_task
    AND EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=OLD.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_verified_releases WHERE attempt_id=OLD.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_replacement_releases WHERE attempt_id=OLD.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_stopped_dispositions WHERE attempt_id=OLD.attempt_id)
    AND NOT EXISTS(SELECT 1 FROM profiled_auto_settlements a WHERE a.attempt_id=OLD.attempt_id AND a.disposition='confirmed' AND a.receipt IS NOT NULL)
    AND NOT profiled_never_granted_valid(OLD.attempt_id)
 THEN RAISE EXCEPTION 'actual owner-verified release record required' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
UPDATE meta SET value='66' WHERE key='state_format_version';
