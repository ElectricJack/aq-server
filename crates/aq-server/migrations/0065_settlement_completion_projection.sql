-- Refresh the compatibility attempt snapshot only from confirmed settlement
-- evidence. Live authority and the immutable launch fields remain unchanged.
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
    AND NOT EXISTS(SELECT 1 FROM profiled_auto_settlements a WHERE a.attempt_id=OLD.attempt_id AND a.disposition='confirmed' AND a.receipt IS NOT NULL)
    AND NOT profiled_never_granted_valid(OLD.attempt_id)
 THEN RAISE EXCEPTION 'actual owner-verified release record required' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
-- The terminal payload remains frozen; its source ACK may advance once.
CREATE OR REPLACE FUNCTION settlement_completion_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE e profiled_settlement_evaluations%ROWTYPE; l JSONB; p JSONB;
BEGIN
 IF TG_OP='DELETE' THEN RAISE EXCEPTION 'retained completion intent'; END IF;
 IF current_setting('aq.settlement_owner',true) IS DISTINCT FROM 'aq.settlement-owner.1' THEN RAISE EXCEPTION 'actual completion owner required'; END IF;
 SELECT * INTO STRICT e FROM profiled_settlement_evaluations WHERE operation_id=NEW.evaluation_operation_id;
 SELECT execution_spec INTO STRICT l FROM profiled_launches WHERE attempt_id=NEW.attempt_id;
 p:=convert_from(NEW.payload_bytes,'UTF8')::JSONB;
 IF e.disposition<>'accepted' OR e.attempt_id<>NEW.attempt_id OR e.release_id<>NEW.release_id OR e.receipt->>'completion_operation_id' IS DISTINCT FROM NEW.operation_id::TEXT
 OR NEW.workspace_id::TEXT IS DISTINCT FROM l#>>'{task,workspace_id}' OR NEW.task_id::TEXT IS DISTINCT FROM l#>>'{task,task_id}'
 OR NEW.expected_version::TEXT IS DISTINCT FROM l#>>'{task,version}' OR NEW.expected_content_revision::TEXT IS DISTINCT FROM l#>>'{task,content_revision}'
 OR p IS DISTINCT FROM jsonb_build_object('idempotency_key','aq-settlement-v1:'||NEW.operation_id::TEXT,'reason','Human-reviewed retained result '||e.report_id::TEXT,'operations',jsonb_build_array(jsonb_build_object('op','update_node','id',NEW.task_id,'expected_version',NEW.expected_version,'patch',jsonb_build_object('task_state','completed'))))
 THEN RAISE EXCEPTION 'completion cannot retarget accepted original task'; END IF;
 IF TG_OP='INSERT' THEN
  IF NEW.state<>'prepared' OR NEW.dispatch_count<>0 OR NEW.receipt IS NOT NULL OR NEW.outbox IS NOT NULL OR NEW.lease_id IS NOT NULL THEN RAISE EXCEPTION 'new completion must be undispatched'; END IF;
 ELSE
  IF (to_jsonb(NEW)-ARRAY['state','dispatch_count','lease_id','lease_expires_at_ms','receipt','outbox','outbox_acknowledged']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','dispatch_count','lease_id','lease_expires_at_ms','receipt','outbox','outbox_acknowledged'])
   OR NEW.dispatch_count<OLD.dispatch_count OR NEW.dispatch_count>OLD.dispatch_count+1
   OR (OLD.outbox_acknowledged AND NOT NEW.outbox_acknowledged)
   OR (OLD.state IN ('confirmed','conflict','blocked') AND (to_jsonb(NEW)-'outbox_acknowledged') IS DISTINCT FROM (to_jsonb(OLD)-'outbox_acknowledged'))
   OR (NEW.dispatch_count=OLD.dispatch_count+1 AND (NEW.state<>'possible' OR NEW.lease_id IS NULL OR NEW.lease_expires_at_ms IS NULL))
   OR (NEW.dispatch_count=OLD.dispatch_count AND NEW.lease_id IS NOT NULL AND ROW(NEW.lease_id,NEW.lease_expires_at_ms) IS DISTINCT FROM ROW(OLD.lease_id,OLD.lease_expires_at_ms))
   OR (NEW.state IS DISTINCT FROM OLD.state AND NEW.state IN ('confirmed','conflict') AND (OLD.state<>'possible' OR OLD.lease_id IS NULL OR NEW.lease_id IS NOT NULL))
  THEN RAISE EXCEPTION 'completion transition or immutable intent changed'; END IF;
 END IF;
 IF NEW.state='confirmed' AND (NEW.receipt->>'workspace_id' IS DISTINCT FROM NEW.workspace_id::TEXT OR NEW.receipt->>'task_id' IS DISTINCT FROM NEW.task_id::TEXT
  OR NEW.receipt->>'version' IS DISTINCT FROM (NEW.expected_version+1)::TEXT OR NEW.receipt->>'content_revision' IS DISTINCT FROM (NEW.expected_content_revision+1)::TEXT
  OR COALESCE(NEW.receipt->>'change_set_id','') !~ '^[0-9a-f-]{36}$' OR COALESCE(NEW.receipt->>'response_digest','') !~ '^[0-9a-f]{64}$'
  OR NEW.outbox->>'event_id' IS DISTINCT FROM NEW.operation_id::TEXT OR NEW.outbox->>'producer' IS DISTINCT FROM 'aq.task.settlement@1'
  OR NEW.outbox->>'event' IS DISTINCT FROM 'task.completed' OR NEW.outbox->>'attempt_id' IS DISTINCT FROM NEW.attempt_id::TEXT
  OR NEW.outbox->>'evaluation_operation_id' IS DISTINCT FROM NEW.evaluation_operation_id::TEXT OR NEW.outbox->'report' IS DISTINCT FROM e.receipt->'report'
  OR NEW.outbox->>'release_id' IS DISTINCT FROM NEW.release_id::TEXT OR NEW.outbox->'receipt' IS DISTINCT FROM NEW.receipt)
 THEN RAISE EXCEPTION 'completion receipt/outbox must match actual original task'; END IF;
 RETURN NEW;
END $$;
-- Separate catalog identity: existing catalog 1/2/3 fingerprints stay fixed.
ALTER TABLE playbook_outbox DROP CONSTRAINT playbook_outbox_producer_check;
ALTER TABLE playbook_outbox ADD CHECK(producer IN ('aq.host.bridge@1','aq.human.decisions@1','aq.graph.operations@1','aq.task.settlement@1'));
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3,4));
UPDATE meta SET value='65' WHERE key='state_format_version';
