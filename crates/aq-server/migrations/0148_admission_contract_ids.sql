-- The admission contract of a proposal is checked by the typed Rust reader
-- against the contract artifact the server computes from its own schema. The
-- two SQL owners below repeated that check against digest literals, so every
-- contract change needed a migration. They now key on the contract id (which
-- names the profile and its origin kind); the digest stays in the stored
-- document. Rows recorded under an earlier digest of the same id stay
-- readable, and no stored row is touched. The phase-terminal contract pins
-- are unchanged. Everything else in both bodies is the text of 0041 and 0043.
SET LOCAL lock_timeout = '5s';
CREATE OR REPLACE FUNCTION profiled_result_origin_valid(attempt UUID,owner_node UUID,origin_kind TEXT,execution_digest TEXT,stopped_ack_digest TEXT) RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
 SELECT EXISTS(
  SELECT 1 FROM profiled_phase_execution_grants g
  JOIN profiled_v2_proposals p USING(attempt_id)
  JOIN profiled_started_receipts started USING(attempt_id)
  JOIN profiled_stopped_receipts stopped USING(attempt_id)
  WHERE g.attempt_id=attempt AND g.node_id=owner_node AND started.node_id=owner_node AND stopped.node_id=owner_node
   AND g.receipt_digest=execution_digest AND stopped.phase_ack_digest=stopped_ack_digest
   AND started.initial_authority_kind='phase_execution_v2' AND stopped.initial_authority_kind='phase_execution_v2'
   AND started.initial_authority_receipt_digest=g.receipt_digest AND stopped.initial_authority_receipt_digest=g.receipt_digest
   AND started.phase_terminal_qualification=stopped.phase_terminal_qualification
   AND convert_from(started.phase_envelope_bytes,'UTF8')::JSONB->'negotiation'=convert_from(stopped.phase_envelope_bytes,'UTF8')::JSONB->'negotiation'
   AND convert_from(started.phase_ack_bytes,'UTF8')::JSONB->'negotiation'=convert_from(started.phase_envelope_bytes,'UTF8')::JSONB->'negotiation'
   AND convert_from(stopped.phase_ack_bytes,'UTF8')::JSONB->'negotiation'=convert_from(stopped.phase_envelope_bytes,'UTF8')::JSONB->'negotiation'
   AND convert_from(stopped.phase_ack_bytes,'UTF8')::JSONB#>>'{origin,execution_receipt_digest}'=g.receipt_digest
   AND convert_from(stopped.phase_ack_bytes,'UTF8')::JSONB#>>'{origin,accepted_receipt_digest}'=g.accepted_receipt_digest
   AND convert_from(stopped.phase_ack_bytes,'UTF8')::JSONB->'original'=convert_from(stopped.receipt_bytes,'UTF8')::JSONB
   AND ((origin_kind='phase_execution_v2' AND p.proposal#>>'{negotiation,contract,id}' IN ('aq.admission.preparation-v2-contract','aq.admission.preparation-v2-contract.interactive-long-v1') AND convert_from(stopped.phase_ack_bytes,'UTF8')::JSONB#>'{negotiation,contract}'='{"digest":"292c30eba95b8dd6f1d9a7bf75775bc5d0f9ecb54644047747f9593d538c0a4a","id":"aq.phase-terminal.contract","version":"1"}'::JSONB)
    OR (origin_kind='phase_refresh_execution_v1' AND p.proposal#>>'{negotiation,contract,id}' IN ('aq.admission.preparation-registry-refresh-v1-contract','aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1') AND convert_from(stopped.phase_ack_bytes,'UTF8')::JSONB#>'{negotiation,contract}'='{"digest":"f9af55c49127a8cb93f03ef5bbb28313d13f94a221affa3e26d91d45bffd6c2d","id":"aq.phase-terminal.registry-refresh-v1.contract","version":"1"}'::JSONB))
 )
$$;

CREATE OR REPLACE FUNCTION profiled_close_ungranted(p_node UUID,p_attempt UUID,p_now BIGINT,p_closure UUID,p_operation UUID,p_cancel BOOLEAN)
RETURNS TABLE(body_bytes BYTEA,body_digest TEXT,changed BOOLEAN) LANGUAGE plpgsql AS $$
DECLARE a execution_attempts%ROWTYPE; h profiled_authority_hot%ROWTYPE;
 l profiled_launches%ROWTYPE; deadline BIGINT; why TEXT; ids JSONB;
 doc JSONB; raw BYTEA; hashed TEXT; lease_count INTEGER; accepted_plan JSONB; pending_ids JSONB; pending_count INTEGER; phase_accept profiled_v2_accepts%ROWTYPE; phase_proposal profiled_v2_proposals%ROWTYPE; phase_body JSONB;
BEGIN
 IF p_now<=0 OR p_closure='00000000-0000-0000-0000-000000000000'::uuid OR p_operation='00000000-0000-0000-0000-000000000000'::uuid
 THEN RAISE EXCEPTION 'invalid internal closure identity/time' USING ERRCODE='23514'; END IF;
 PERFORM 1 FROM nodes WHERE id=p_node FOR UPDATE;
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
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='148' WHERE key='state_format_version';
