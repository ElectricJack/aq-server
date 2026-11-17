-- Occupancy is a recorded fact, not a proof re-run on every read. Until now
-- every slot poll and every Swarm read re-validated the stop proof of each
-- held attempt (an eight-table join PostgreSQL planned again on every call).
-- The proof is now validated once, when the stop is accepted, and its answer
-- kept: a row here says "this attempt no longer holds compute on its node".
CREATE TABLE execution_attempt_compute_releases (
 attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
 node_id UUID NOT NULL,
 proof TEXT NOT NULL CHECK (proof IN ('stopped','lost_release')),
 stop_receipt_digest TEXT NOT NULL CHECK (stop_receipt_digest ~ '^[0-9a-f]{64}$'),
 -- The typed stop witnesses (origin kind, execution receipt, stopped acknowledgement) the
 -- server's stop validator must present to reuse the compute of a stopped attempt.
 witnesses JSONB NOT NULL CHECK (jsonb_typeof(witnesses)='array')
);
-- Only `native_compute_release_record` writes a release, after it validated the proof.
CREATE FUNCTION execution_attempt_compute_release_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' THEN
  IF EXISTS(SELECT 1 FROM execution_attempts WHERE attempt_id=OLD.attempt_id) THEN
   RAISE EXCEPTION 'compute releases are immutable';
  END IF;
  RETURN OLD;
 END IF;
 IF TG_OP='UPDATE' OR current_setting('aq.compute_release_owner',true) IS DISTINCT FROM '1' THEN
  RAISE EXCEPTION 'compute releases are recorded only with their validated stop proof';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER execution_attempt_compute_release_guard BEFORE INSERT OR UPDATE OR DELETE
 ON execution_attempt_compute_releases FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_release_guard();

-- Validates the stop proof of a finished attempt (the original stopped receipt
-- chain, or its lost-attempt release) and records the release. Idempotent;
-- false while the attempt's stop is not proven.
CREATE FUNCTION native_compute_release_record(attempt UUID) RETURNS BOOLEAN LANGUAGE plpgsql AS $$
DECLARE owner UUID; stop JSONB; kind TEXT; pins JSONB:='[]'::JSONB;
BEGIN
 SELECT a.node_id,h.state->'stop' INTO owner,stop
 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id)
 WHERE a.attempt_id=attempt AND a.kind='profiled_v1' AND h.state->>'status'='finished'
  AND h.state#>>'{stop,kind}'='stopped';
 IF NOT FOUND THEN RETURN FALSE; END IF;
 IF EXISTS(SELECT 1 FROM execution_attempt_compute_releases r WHERE r.attempt_id=attempt) THEN RETURN TRUE; END IF;
 IF EXISTS(SELECT 1 FROM lost_attempt_releases l WHERE l.attempt_id=attempt AND l.node_id=owner
  AND stop=jsonb_build_object('kind','stopped','receipt',jsonb_build_object(
   'id','aq.lost-attempt-release/'||attempt::text,'version','1','digest',l.receipt_digest))) THEN
  kind:='lost_release';
 ELSIF profiled_compute_stopped_valid(attempt,owner) THEN
  kind:='stopped';
  SELECT COALESCE(jsonb_agg(jsonb_build_object('kind',CASE p.proposal#>>'{negotiation,contract,id}'
    WHEN 'aq.admission.preparation-v2-contract' THEN 'phase_execution_v2'
    WHEN 'aq.admission.preparation-v2-contract.interactive-long-v1' THEN 'phase_execution_v2'
    WHEN 'aq.admission.preparation-registry-refresh-v1-contract' THEN 'phase_refresh_execution_v1'
    WHEN 'aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1' THEN 'phase_refresh_execution_v1'
    ELSE 'unsupported' END,'execution',g.receipt_digest,'stopped_ack',s.phase_ack_digest)),'[]'::JSONB) INTO pins
  FROM profiled_phase_execution_grants g JOIN profiled_stopped_receipts s USING(attempt_id)
  JOIN profiled_v2_proposals p USING(attempt_id) WHERE g.attempt_id=attempt;
 ELSE
  RETURN FALSE;
 END IF;
 PERFORM set_config('aq.compute_release_owner','1',true);
 INSERT INTO execution_attempt_compute_releases(attempt_id,node_id,proof,stop_receipt_digest,witnesses)
  VALUES(attempt,owner,kind,stop#>>'{receipt,digest}',pins) ON CONFLICT(attempt_id) DO NOTHING;
 PERFORM set_config('aq.compute_release_owner','',true);
 RETURN TRUE;
END $$;

-- The release is recorded by the transaction that accepts the stop (or the
-- lost-attempt release): every stop owner writes its receipts first and the
-- hot projection last, so the proof is complete when this trigger runs.
CREATE FUNCTION native_compute_release_on_stop() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 PERFORM native_compute_release_record(NEW.attempt_id);
 RETURN NULL;
END $$;
CREATE TRIGGER native_compute_release_on_stop AFTER INSERT OR UPDATE ON profiled_authority_hot
 FOR EACH ROW
 WHEN (NEW.state->>'status'='finished' AND NEW.state#>>'{stop,kind}'='stopped')
 EXECUTE FUNCTION native_compute_release_on_stop();

-- Occupancy: a plain read of the claim and the recorded release.
CREATE OR REPLACE FUNCTION native_attempt_holds_compute(attempt UUID, owner_node UUID)
RETURNS BOOLEAN LANGUAGE plpgsql STABLE AS $$
BEGIN
 RETURN EXISTS(SELECT 1 FROM execution_attempts a WHERE a.attempt_id=attempt AND a.node_id=owner_node AND a.holds_task
  AND NOT EXISTS(SELECT 1 FROM execution_attempt_compute_releases r WHERE r.attempt_id=a.attempt_id AND r.node_id=owner_node));
END $$;

-- Reuse past an attempt that still holds its task keeps the typed Rust
-- stop-validation witness of the original fence: the transaction presents the
-- witness (`aq.compute_stopped`) and it must be one recorded with the release.
CREATE OR REPLACE FUNCTION native_compute_reuse_proven(attempt UUID, owner_node UUID)
RETURNS BOOLEAN LANGUAGE plpgsql STABLE AS $$
BEGIN
 RETURN NOT native_attempt_holds_compute(attempt,owner_node)
  AND (NOT EXISTS(SELECT 1 FROM execution_attempts WHERE attempt_id=attempt AND holds_task)
   OR EXISTS(SELECT 1 FROM execution_attempt_compute_releases r WHERE r.attempt_id=attempt AND r.node_id=owner_node
    AND (r.proof='lost_release' OR r.witnesses @> jsonb_build_array(
     COALESCE(NULLIF(current_setting('aq.compute_stopped',true),'')::JSONB,'{}'::JSONB)->attempt::text))));
END $$;

-- Every held attempt whose stop is already proven, in whatever state it settled.
SELECT native_compute_release_record(attempt_id) FROM execution_attempts WHERE holds_task AND kind='profiled_v1';

-- A failed attempt's claim is now released as soon as it is settled, whether
-- or not a retry is admitted. The task's hold (an exhausted budget, a changed
-- project binding) is about the task and stays until a new attempt, an owner's
-- disposition or the task's closure answers it.
CREATE OR REPLACE FUNCTION task_holds_clear() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP = 'INSERT' THEN
  UPDATE task_holds SET cleared_at = now(), cleared_by = 'new_attempt'
   WHERE task_id = NEW.task_id AND cleared_at IS NULL;
 ELSIF OLD.holds_task AND NOT NEW.holds_task
  AND NOT EXISTS(SELECT 1 FROM profiled_replacement_releases r WHERE r.attempt_id = NEW.attempt_id) THEN
  UPDATE task_holds SET cleared_at = now(), cleared_by = 'claim_released'
   WHERE task_id = NEW.task_id AND cleared_at IS NULL;
 END IF;
 RETURN NULL;
END $$;

-- An owner may abandon any held attempt of a revoked node: its credential is
-- gone, so nothing can report its stop, and its launch access is revoked with
-- the disposition. A task may carry one disposition per attempt.
ALTER TABLE profiled_stopped_dispositions DROP CONSTRAINT profiled_stopped_dispositions_task_id_key;
CREATE INDEX profiled_stopped_dispositions_task ON profiled_stopped_dispositions(task_id);
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
   AND encode(sha256(c.receipt_bytes),'hex')=NEW.receipt->>'stop_receipt_digest')
  OR EXISTS(SELECT 1 FROM execution_attempts a JOIN nodes n ON n.id=a.node_id
   WHERE a.attempt_id=NEW.attempt_id AND a.task_id=NEW.task_id AND a.kind='profiled_v1' AND a.holds_task
   AND n.revoked_at IS NOT NULL
   AND NEW.receipt->>'stop_receipt_digest'=encode(sha256(convert_to('aq.node-revoked/'||n.id::text,'UTF8')),'hex')))
 OR EXISTS(SELECT 1 FROM profiled_settlement_evaluations WHERE attempt_id=NEW.attempt_id AND disposition='accepted')
 OR EXISTS(SELECT 1 FROM profiled_task_completions WHERE attempt_id=NEW.attempt_id)
 OR EXISTS(SELECT 1 FROM profiled_auto_settlements WHERE attempt_id=NEW.attempt_id AND disposition IN ('prepared','possible','confirmed'))
 THEN RAISE EXCEPTION 'owner disposition requires stopped unsettled held attempt' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
UPDATE meta SET value='136' WHERE key='state_format_version';
