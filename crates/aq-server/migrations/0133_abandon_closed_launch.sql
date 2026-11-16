-- An owner may also abandon a granted launch that its worker closed before the
-- agent started. Such an attempt has a launch closure (the host's stopped
-- proof) but no stopped receipt. When the closure does not prove that nothing
-- ran (the host had opened a pane) the server keeps the attempt "for an
-- operator", and until now no operator action accepted it: the task stayed
-- "cancelling" and its node could not register again. The disposition's stop
-- proof for such an attempt is the closure receipt.
SET LOCAL lock_timeout = '5s';
CREATE OR REPLACE FUNCTION profiled_stopped_disposition_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'immutable stopped disposition'; END IF;
 IF current_setting('aq.stopped_disposition_owner',true) IS DISTINCT FROM '1'
 OR NOT EXISTS(SELECT 1 FROM users WHERE id=NEW.actor_id AND role='owner')
 OR NOT (EXISTS(SELECT 1 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id)
   JOIN profiled_stopped_receipts s USING(attempt_id)
   WHERE a.attempt_id=NEW.attempt_id AND a.task_id=NEW.task_id AND a.kind='profiled_v1' AND a.holds_task
   AND h.state->>'status'='finished' AND h.state#>>'{stop,kind}'='stopped'
   AND h.state#>>'{stop,receipt,digest}'=NEW.receipt->>'stop_receipt_digest'
   AND encode(sha256(s.receipt_bytes),'hex')=NEW.receipt->>'stop_receipt_digest')
  OR EXISTS(SELECT 1 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id)
   JOIN profiled_launch_closures c USING(attempt_id)
   WHERE a.attempt_id=NEW.attempt_id AND a.task_id=NEW.task_id AND a.kind='profiled_v1' AND a.holds_task
   AND h.state->>'status'='cancelling' AND h.state->>'revoked'='true' AND h.state#>>'{stop,kind}'='unknown'
   AND c.receipt_digest=NEW.receipt->>'stop_receipt_digest'
   AND encode(sha256(c.receipt_bytes),'hex')=NEW.receipt->>'stop_receipt_digest'))
 OR EXISTS(SELECT 1 FROM profiled_settlement_evaluations WHERE attempt_id=NEW.attempt_id AND disposition='accepted')
 OR EXISTS(SELECT 1 FROM profiled_task_completions WHERE attempt_id=NEW.attempt_id)
 OR EXISTS(SELECT 1 FROM profiled_auto_settlements WHERE attempt_id=NEW.attempt_id AND disposition IN ('prepared','possible','confirmed'))
 THEN RAISE EXCEPTION 'owner disposition requires stopped unsettled held attempt' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='133' WHERE key='state_format_version';
