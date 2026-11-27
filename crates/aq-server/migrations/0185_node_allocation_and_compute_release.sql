-- The nodes row is a node's credential: workers check it with FOR KEY SHARE,
-- which heartbeats and other worker transactions do not wait for. Work that
-- must exclude other work on the node (slot allocation, offer publication,
-- manual-session claims, runtime registration, owner fences) locks this row.
-- An owner fence bumps `generation`, so a SERIALIZABLE publication that read
-- the old state fails to serialize instead of publishing past the change.
CREATE TABLE node_allocation (
 node_id UUID PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
 generation BIGINT NOT NULL DEFAULT 1
);
INSERT INTO node_allocation(node_id) SELECT id FROM nodes;

CREATE OR REPLACE FUNCTION initialize_node_slots() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 INSERT INTO worker_settings(kind,id,max_agents,revision)
 SELECT 'node',NEW.id::text,slots,1 FROM node_slot_default;
 INSERT INTO node_allocation(node_id) VALUES(NEW.id);
 RETURN NEW;
END $$;

-- Whether an attempt still holds compute on its node, kept current when it
-- changes instead of re-proved on every capacity read. `released` is always
-- NOT native_attempt_holds_compute(attempt_id,node_id): every table that proof
-- reads recomputes it for the attempt it wrote, so the last write of a
-- transaction sees all of them, whatever order they came in. Readers count a
-- held attempt without a released row (`holds_task AND NOT EXISTS(released)`),
-- so a row written past the triggers fails closed: it still occupies its slot.
CREATE TABLE execution_attempt_compute (
 attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
 node_id UUID NOT NULL,
 released BOOLEAN NOT NULL
);
INSERT INTO execution_attempt_compute
 SELECT attempt_id,node_id,NOT native_attempt_holds_compute(attempt_id,node_id) FROM execution_attempts;

CREATE FUNCTION execution_attempt_compute_refresh() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE target UUID; owner UUID; was BOOLEAN; released_now BOOLEAN;
BEGIN
 IF TG_TABLE_NAME='execution_attempts' AND TG_OP='INSERT' THEN
  INSERT INTO execution_attempt_compute(attempt_id,node_id,released)
   VALUES(NEW.attempt_id,NEW.node_id,NOT native_attempt_holds_compute(NEW.attempt_id,NEW.node_id));
  RETURN NULL;
 END IF;
 IF TG_OP='DELETE' THEN target:=OLD.attempt_id; ELSE target:=NEW.attempt_id; END IF;
 -- Lock first, so the proof below reads every committed write of the attempt.
 SELECT node_id,released INTO owner,was FROM execution_attempt_compute WHERE attempt_id=target FOR UPDATE;
 IF NOT FOUND THEN RETURN NULL; END IF;
 released_now:=NOT native_attempt_holds_compute(target,owner);
 IF released_now IS DISTINCT FROM was THEN
  UPDATE execution_attempt_compute SET released=released_now WHERE attempt_id=target;
 END IF;
 RETURN NULL;
END $$;
CREATE TRIGGER execution_attempt_compute_insert AFTER INSERT ON execution_attempts
 FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_refresh();
CREATE TRIGGER execution_attempt_compute_refresh AFTER UPDATE OF holds_task,node_id,kind ON execution_attempts
 FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_refresh();
CREATE TRIGGER execution_attempt_compute_refresh AFTER INSERT OR UPDATE OR DELETE ON profiled_authority_hot
 FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_refresh();
CREATE TRIGGER execution_attempt_compute_refresh AFTER INSERT OR UPDATE OR DELETE ON lost_attempt_releases
 FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_refresh();
CREATE TRIGGER execution_attempt_compute_refresh AFTER INSERT OR UPDATE OR DELETE ON profiled_launches
 FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_refresh();
CREATE TRIGGER execution_attempt_compute_refresh AFTER INSERT OR UPDATE OR DELETE ON profiled_phase_execution_grants
 FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_refresh();
CREATE TRIGGER execution_attempt_compute_refresh AFTER INSERT OR UPDATE OR DELETE ON profiled_v2_proposals
 FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_refresh();
CREATE TRIGGER execution_attempt_compute_refresh AFTER INSERT OR UPDATE OR DELETE ON profiled_started_receipts
 FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_refresh();
CREATE TRIGGER execution_attempt_compute_refresh AFTER INSERT OR UPDATE OR DELETE ON profiled_stopped_receipts
 FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_refresh();
CREATE TRIGGER execution_attempt_compute_refresh AFTER INSERT OR UPDATE OR DELETE ON profiled_owner_journals
 FOR EACH ROW EXECUTE FUNCTION execution_attempt_compute_refresh();

-- Same checks as 0135; the node's allocation row replaces the global advisory
-- lock and the nodes row, and the capacity count reads the stored release.
CREATE OR REPLACE FUNCTION node_slot_assignment_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE owner UUID; index_value INTEGER; maximum INTEGER; excluded_attempt UUID; excluded_session UUID;
BEGIN
 IF TG_OP='DELETE' THEN
  IF TG_TABLE_NAME='execution_attempt_slots' THEN
   IF EXISTS(SELECT 1 FROM execution_attempts WHERE attempt_id=OLD.attempt_id) THEN
    RAISE EXCEPTION 'slot assignments are immutable';
   END IF;
  ELSE
   IF EXISTS(SELECT 1 FROM manual_agent_sessions WHERE id=OLD.session_id) THEN
    RAISE EXCEPTION 'slot assignments are immutable';
   END IF;
  END IF;
  RETURN OLD;
 END IF;
 IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'slot assignments are immutable'; END IF;
 owner:=NEW.node_id; index_value:=NEW.slot;
 -- Same lock as publication and manual claims, which take it first.
 PERFORM 1 FROM node_allocation WHERE node_id=owner FOR UPDATE;
 IF NOT FOUND THEN RAISE EXCEPTION 'slot node does not exist'; END IF;
 SELECT COALESCE(s.max_agents,d.slots) INTO maximum FROM node_slot_default d
 LEFT JOIN worker_settings s ON s.kind='node' AND s.id=owner::text;
 IF index_value>maximum THEN RAISE EXCEPTION 'node slot count reached'; END IF;
 IF TG_TABLE_NAME='execution_attempt_slots' THEN
  excluded_attempt:=NEW.attempt_id;
  IF NOT EXISTS(SELECT 1 FROM execution_attempts a WHERE a.attempt_id=excluded_attempt AND a.node_id=owner)
  THEN RAISE EXCEPTION 'slot attempt scope mismatch'; END IF;
 ELSE
  excluded_session:=NEW.session_id;
  IF NOT EXISTS(SELECT 1 FROM manual_agent_sessions m WHERE m.id=excluded_session AND m.node_id=owner)
  THEN RAISE EXCEPTION 'slot session scope mismatch'; END IF;
 END IF;
 IF EXISTS(SELECT 1 FROM execution_attempts a LEFT JOIN execution_attempt_slots s USING(attempt_id)
  WHERE a.node_id=owner AND a.attempt_id IS DISTINCT FROM excluded_attempt AND a.holds_task
   AND (a.kind<>'profiled_v1' OR s.slot IS NULL OR s.slot=index_value)
   AND NOT native_compute_reuse_proven(a.attempt_id,owner))
 OR EXISTS(SELECT 1 FROM manual_agent_sessions m LEFT JOIN manual_agent_slots s ON s.session_id=m.id
  WHERE m.node_id=owner AND m.id IS DISTINCT FROM excluded_session
   AND m.stopped_at_ms IS NULL AND m.status<>'pending' AND (s.slot IS NULL OR s.slot=index_value))
 THEN RAISE EXCEPTION 'node slot remains occupied'; END IF;
 IF (SELECT count(*) FROM execution_attempts a WHERE a.node_id=owner
      AND a.attempt_id IS DISTINCT FROM excluded_attempt AND a.holds_task
      AND NOT EXISTS(SELECT 1 FROM execution_attempt_compute c WHERE c.attempt_id=a.attempt_id AND c.released))
  +(SELECT count(*) FROM manual_agent_sessions m WHERE m.node_id=owner
      AND m.id IS DISTINCT FROM excluded_session AND m.stopped_at_ms IS NULL AND m.status<>'pending')>=maximum
 THEN RAISE EXCEPTION 'node capacity reached'; END IF;
 RETURN NEW;
END $$;

-- Credential checks inside SQL owners take the same FOR KEY SHARE as their
-- callers: a FOR UPDATE here would upgrade the caller's lock and deadlock
-- two transactions of one node. The attempt row locked next orders the work.
CREATE OR REPLACE FUNCTION profiled_close_ungranted(p_node UUID,p_attempt UUID,p_now BIGINT,p_closure UUID,p_operation UUID,p_cancel BOOLEAN)
RETURNS TABLE(body_bytes BYTEA,body_digest TEXT,changed BOOLEAN) LANGUAGE plpgsql AS $$
DECLARE a execution_attempts%ROWTYPE; h profiled_authority_hot%ROWTYPE;
 l profiled_launches%ROWTYPE; deadline BIGINT; why TEXT; ids JSONB;
 doc JSONB; raw BYTEA; hashed TEXT; lease_count INTEGER; accepted_plan JSONB; pending_ids JSONB; pending_count INTEGER; phase_accept profiled_v2_accepts%ROWTYPE; phase_proposal profiled_v2_proposals%ROWTYPE; phase_body JSONB;
BEGIN
 IF p_now<=0 OR p_closure='00000000-0000-0000-0000-000000000000'::uuid OR p_operation='00000000-0000-0000-0000-000000000000'::uuid
 THEN RAISE EXCEPTION 'invalid internal closure identity/time' USING ERRCODE='23514'; END IF;
 PERFORM 1 FROM nodes WHERE id=p_node FOR KEY SHARE;
 SELECT * INTO STRICT a FROM execution_attempts WHERE attempt_id=p_attempt AND node_id=p_node FOR UPDATE;
 IF a.kind<>'profiled_v1' THEN RAISE EXCEPTION 'native ungranted owner only' USING ERRCODE='23514'; END IF;
 SELECT * INTO STRICT h FROM profiled_authority_hot WHERE attempt_id=p_attempt FOR UPDATE;
 SELECT c.proof_bytes,c.proof_digest INTO raw,hashed FROM profiled_never_granted_closures c WHERE c.attempt_id=p_attempt;
 IF FOUND THEN
   IF a.holds_task OR NOT profiled_never_granted_valid(p_attempt) THEN RAISE EXCEPTION 'closure proof/current authority conflict' USING ERRCODE='23514'; END IF;
   RETURN QUERY SELECT raw,hashed,FALSE; RETURN;
 END IF;
 SELECT * INTO STRICT l FROM profiled_launches WHERE attempt_id=p_attempt FOR SHARE;
 IF l.launch_binding IS DISTINCT FROM a.state#>'{offer,binding}'
    OR l.launch_binding->>'attempt_id' IS DISTINCT FROM a.attempt_id::text
    OR l.launch_binding->>'offer_id' IS DISTINCT FROM a.offer_id::text
    OR l.launch_binding#>>'{session,node_id}' IS DISTINCT FROM a.node_id::text
    OR l.launch_binding#>>'{session,attempt_id}' IS DISTINCT FROM a.attempt_id::text
    OR l.launch_binding#>>'{session,session_id}' IS DISTINCT FROM a.session_id::text
 THEN RAISE EXCEPTION 'actual sealed/common assignment binding mismatch' USING ERRCODE='23514'; END IF;
 IF NOT a.holds_task OR h.state->'grant'<>'null'::jsonb OR h.state#>>'{stop,kind}'<>'unknown'
 THEN RAISE EXCEPTION 'issued/unknown-start or released authority cannot use no-grant closure' USING ERRCODE='23514'; END IF;
 IF h.state->>'status'='offered' THEN deadline:=l.offer_expires_at_ms; why:='offer_expired';
 ELSIF h.state->>'status'='preparing' THEN
   SELECT * INTO phase_accept FROM profiled_v2_accepts WHERE attempt_id=p_attempt FOR SHARE;
   IF FOUND THEN
     -- Typed Rust history validation precedes this internal SQL owner. No
     -- worker assertion or fabricated legacy PreparationPlan is accepted.
     IF current_setting('aq.phase_no_grant_owner',true) IS DISTINCT FROM 'aq.phase-no-grant-owner.1'
     THEN RAISE EXCEPTION 'typed phase absence owner required' USING ERRCODE='23514'; END IF;
     SELECT * INTO STRICT phase_proposal FROM profiled_v2_proposals WHERE attempt_id=p_attempt FOR SHARE;
     phase_body:=phase_accept.receipt;
     IF phase_body#>>'{negotiation,contract,id}' IS NULL
       OR phase_body#>>'{negotiation,contract,id}' NOT IN ('aq.admission.preparation-v2-contract','aq.admission.preparation-registry-refresh-v1-contract','aq.admission.preparation-v2-contract.interactive-long-v1','aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1')
       OR phase_accept.node_id IS DISTINCT FROM p_node
       OR phase_proposal.node_id IS DISTINCT FROM p_node
       OR phase_proposal.boot_id::TEXT IS DISTINCT FROM l.launch_binding#>>'{session,boot_id}'
       OR phase_proposal.proposal#>'{original,offered,offer,binding}' IS DISTINCT FROM l.launch_binding
       OR phase_accept.operation_id::TEXT IS DISTINCT FROM h.state->>'preparation_operation_id'
       OR phase_accept.request#>'{original,acceptance,binding}' IS DISTINCT FROM l.launch_binding
       OR phase_body#>'{lease,binding}' IS DISTINCT FROM l.launch_binding
       OR phase_body#>>'{lease,guardian_id}' IS DISTINCT FROM h.state#>>'{owner,guardian_id}'
       OR phase_body#>>'{lease,launch_id}' IS DISTINCT FROM h.state#>>'{owner,launch_id}'
       OR phase_body#>>'{lease,hold,reservation,reservation_id}' IS DISTINCT FROM h.state#>>'{owner,reservation_id}'
       OR phase_body->'negotiation' IS DISTINCT FROM phase_accept.request->'negotiation'
       OR phase_body->'negotiation' IS DISTINCT FROM phase_proposal.proposal->'negotiation'
       OR (phase_body#>>'{lease,issued_at_ms}')::BIGINT IS DISTINCT FROM phase_accept.issued_at_ms
       OR (phase_body#>>'{lease,expires_at_ms}')::BIGINT IS DISTINCT FROM phase_accept.expires_at_ms
       OR phase_body->>'operation_id' IS DISTINCT FROM phase_accept.operation_id::TEXT
       OR phase_body->>'request_digest' IS DISTINCT FROM phase_accept.request_digest
       OR (phase_body#>>'{versions,grant_revision}')::BIGINT IS DISTINCT FROM 0
       OR phase_accept.expires_at_ms-phase_accept.issued_at_ms NOT BETWEEN 1 AND 120000
       OR NOT EXISTS(SELECT 1 FROM admission_material_leases m JOIN profiled_launch_materials x ON x.lease_id=m.lease_id
          WHERE x.attempt_id=p_attempt AND m.lease_id=phase_accept.lease_id AND m.prospective_attempt_id=p_attempt
          AND m.admitted_attempt_id=p_attempt AND m.node_id=p_node AND m.boot_id=phase_proposal.boot_id
          AND m.lease=phase_body#>'{lease,materials,0}')
     THEN RAISE EXCEPTION 'actual phase acceptance absence join refused' USING ERRCODE='23514'; END IF;
     deadline:=phase_accept.expires_at_ms;
   ELSE
   SELECT response->'body' INTO accepted_plan FROM profiled_execution_receipts
    WHERE node_id=p_node AND operation_id=(h.state->>'preparation_operation_id')::UUID
      AND kind='accept' AND response->>'kind'='preparation_plan';
   IF accepted_plan IS NULL OR accepted_plan->'binding' IS DISTINCT FROM l.launch_binding
      OR accepted_plan->>'operation_id' IS DISTINCT FROM h.state->>'preparation_operation_id'
      OR accepted_plan->>'guardian_id' IS DISTINCT FROM h.state#>>'{owner,guardian_id}'
      OR accepted_plan->>'launch_id' IS DISTINCT FROM h.state#>>'{owner,launch_id}'
      OR accepted_plan#>>'{reservation,reservation_id}' IS DISTINCT FROM h.state#>>'{owner,reservation_id}'
      OR jsonb_typeof(accepted_plan->'materials') IS DISTINCT FROM 'array' OR jsonb_array_length(accepted_plan->'materials') NOT BETWEEN 1 AND 16
      OR COALESCE((accepted_plan#>>'{clock,valid_for_ms}')::BIGINT,0) NOT BETWEEN 1 AND 120000
      OR COALESCE((accepted_plan#>>'{clock,server_time_ms}')::BIGINT,0)<=0
      OR (accepted_plan#>>'{clock,server_time_ms}')::BIGINT+(accepted_plan#>>'{clock,valid_for_ms}')::BIGINT IS DISTINCT FROM (accepted_plan#>>'{clock,expires_at_ms}')::BIGINT
   THEN RAISE EXCEPTION 'accepted preparation authority unavailable/unsupported' USING ERRCODE='23514'; END IF;
   deadline:=(accepted_plan#>>'{clock,expires_at_ms}')::BIGINT;
   END IF;
   why:='preparation_expired';
 ELSE RAISE EXCEPTION 'not an expirable pre-grant state' USING ERRCODE='23514'; END IF;
 IF deadline IS NULL THEN RAISE EXCEPTION 'preparation deadline unavailable' USING ERRCODE='23514'; END IF;
 IF p_cancel THEN
   IF NOT EXISTS(SELECT 1 FROM profiled_cancellations c WHERE c.attempt_id=p_attempt
      AND c.cancellation_id=p_operation AND c.cancellation_id::TEXT=h.state->>'cancellation_id'
      AND c.binding=l.launch_binding AND c.recorded_at_ms<=p_now)
   THEN RAISE EXCEPTION 'actual cancellation owner required' USING ERRCODE='23514'; END IF;
   why:='cancelled';
   -- This boundary is cancellation time, never a fictitious expired lease.
   SELECT recorded_at_ms INTO deadline FROM profiled_cancellations WHERE attempt_id=p_attempt;
 ELSIF p_now<deadline THEN RETURN; END IF;
 IF EXISTS(SELECT 1 FROM profiled_execution_receipts WHERE node_id=p_node AND kind IN ('prepare','renew') AND response->>'kind'='grant' AND response#>>'{body,grant,binding,attempt_id}'=p_attempt::text)
    OR NOT profiled_no_execution_effects(p_attempt)
 THEN RAISE EXCEPTION 'actual grant/source/effect records contradict no-execution proof' USING ERRCODE='23514'; END IF;
 PERFORM 1 FROM mailbox_deliveries WHERE attempt_id=p_attempt ORDER BY delivery_id FOR UPDATE;
 SELECT count(*),COALESCE(jsonb_agg(delivery_id ORDER BY delivery_id),'[]'::jsonb) INTO pending_count,pending_ids FROM mailbox_deliveries WHERE attempt_id=p_attempt AND disposition='pending';
 IF pending_count>256 THEN RAISE EXCEPTION 'no-grant pending delivery closure bound' USING ERRCODE='23514'; END IF;
 IF NOT profiled_no_execution_effects(p_attempt) THEN RAISE EXCEPTION 'delivery uncertainty contradicts no-execution proof' USING ERRCODE='23514'; END IF;
 UPDATE mailbox_deliveries SET disposition='stale',updated_at_ms=p_now WHERE attempt_id=p_attempt AND disposition='pending';
 PERFORM 1 FROM admission_material_leases WHERE prospective_attempt_id=p_attempt ORDER BY lease_id FOR UPDATE;
 SELECT count(*),COALESCE(jsonb_agg(lease_id ORDER BY lease_id),'[]'::jsonb) INTO lease_count,ids FROM admission_material_leases WHERE prospective_attempt_id=p_attempt;
 IF lease_count>64 THEN RAISE EXCEPTION 'no-grant material history bound' USING ERRCODE='23514'; END IF;
 UPDATE admission_material_leases SET state='revoked' WHERE prospective_attempt_id=p_attempt AND state IN ('pending','admitted');
 UPDATE agent_launch_bindings SET revoked=TRUE,credential_verifier=NULL,credential_expires_at_ms=NULL,credential_version=credential_version+1,authority_revision=authority_revision+1 WHERE attempt_id=p_attempt;
 doc:=jsonb_build_object('schema_version',1,'closure_id',p_closure,'operation_id',p_operation,
  'binding',l.launch_binding,'observed_record_revision',h.record_revision,'closed_at_ms',p_now,'reason',why,
  'applicable_deadline_ms',deadline,'material_leases_revoked',ids,'pending_deliveries_staled',pending_ids,'pending_deliveries_staled_count',pending_count,
  'source_checkpoint',jsonb_build_object('task_checkpoint_transfers',0),
  'effect_checkpoint',jsonb_build_object('graph_operations',0,'created_records',0,'observation_sources',0,'observation_events',0,'mailbox_receipts',0,'uncertain_deliveries',0,'session_messages',0));
 raw:=convert_to(doc::text,'UTF8'); hashed:=encode(sha256(raw),'hex');
 INSERT INTO profiled_never_granted_closures(attempt_id,closure_id,operation_id,binding,observed_record_revision,closed_at_ms,reason,proof_bytes,proof_digest)
 VALUES(p_attempt,p_closure,p_operation,l.launch_binding,h.record_revision,p_now,why,raw,hashed);
 UPDATE profiled_authority_hot SET state=jsonb_set(state,'{status}','"never_granted"') WHERE attempt_id=p_attempt;
 UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=p_attempt;
 RETURN QUERY SELECT raw,hashed,TRUE;
END $$;
CREATE OR REPLACE FUNCTION delegated_permit_guard() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN
  RAISE EXCEPTION 'delegated issuance is retained' USING ERRCODE='23514';
 ELSIF TG_OP='UPDATE' THEN
  IF (to_jsonb(NEW)-'revoked') IS DISTINCT FROM (to_jsonb(OLD)-'revoked') OR (OLD.revoked AND NOT NEW.revoked)
  THEN RAISE EXCEPTION 'delegated issuance is immutable' USING ERRCODE='23514'; END IF;
 ELSE
  PERFORM 1 FROM nodes WHERE id=NEW.node_id AND revoked_at IS NULL FOR KEY SHARE;
  IF NOT FOUND THEN RAISE EXCEPTION 'delegated node unavailable' USING ERRCODE='23514'; END IF;
  PERFORM 1 FROM execution_attempts WHERE attempt_id=NEW.attempt_id AND node_id=NEW.node_id FOR UPDATE;
  IF NOT FOUND OR EXISTS(SELECT 1 FROM profiled_cancellations WHERE attempt_id=NEW.attempt_id)
  THEN RAISE EXCEPTION 'delegated attempt unavailable' USING ERRCODE='23514'; END IF;
 END IF;
 RETURN NEW;
END $$;

UPDATE meta SET value='185' WHERE key='state_format_version';
