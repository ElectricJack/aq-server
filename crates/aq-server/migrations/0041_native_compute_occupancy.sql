-- Compute reuse is not task/source/result release. Existing held task rows and
-- every old migration remain unchanged. Legacy or incomplete stop stays busy.
CREATE FUNCTION profiled_compute_stopped_valid(attempt UUID, owner_node UUID)
RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
 SELECT COALESCE(EXISTS(
  SELECT 1 FROM execution_attempts a
  JOIN profiled_launches l USING(attempt_id)
  JOIN profiled_authority_hot h USING(attempt_id)
  JOIN profiled_phase_execution_grants g USING(attempt_id)
  JOIN profiled_v2_proposals p USING(attempt_id)
  JOIN profiled_started_receipts started USING(attempt_id)
  JOIN profiled_stopped_receipts stopped USING(attempt_id)
  JOIN profiled_owner_journals j USING(attempt_id)
  WHERE a.attempt_id=attempt AND a.node_id=owner_node AND a.kind='profiled_v1'
   AND g.node_id=owner_node AND h.state->>'status'='finished'
   AND h.state#>>'{grant,grant_id}'=g.grant_id::TEXT
   AND l.launch_binding=convert_from(g.receipt_bytes,'UTF8')::JSONB#>'{grant,binding}'
   AND j.binding=l.launch_binding AND j.phase='stopped' AND j.grant_id=g.grant_id
   AND h.state#>>'{owner,guardian_id}'=j.guardian_id::TEXT
   AND h.state#>>'{owner,launch_id}'=j.launch_id::TEXT
   AND h.state#>>'{owner,reservation_id}'=j.reservation_id::TEXT
   AND h.state->'stop'=jsonb_build_object('kind','stopped','receipt',jsonb_build_object(
    'id','aq.native-stopped/'||attempt::TEXT,'version','1','digest',encode(sha256(stopped.receipt_bytes),'hex')))
   AND convert_from(stopped.request_bytes,'UTF8')::JSONB#>'{evidence,binding}'=l.launch_binding
   AND convert_from(stopped.request_bytes,'UTF8')::JSONB#>>'{evidence,disposition,kind}'='stopped'
   AND convert_from(stopped.request_bytes,'UTF8')::JSONB#>>'{evidence,disposition,grant_id}'=g.grant_id::TEXT
   AND convert_from(stopped.request_bytes,'UTF8')::JSONB#>>'{evidence,journal,kind}'='recorded'
   AND convert_from(stopped.request_bytes,'UTF8')::JSONB#>>'{evidence,journal,revision}'=j.revision::TEXT
   AND convert_from(stopped.request_bytes,'UTF8')::JSONB#>>'{evidence,journal,digest}'=j.digest
   AND convert_from(stopped.proof_bytes,'UTF8')::JSONB=convert_from(stopped.request_bytes,'UTF8')::JSONB->'proof'
   AND convert_from(stopped.proof_bytes,'UTF8')::JSONB#>>'{observation,phase}'='stopped'
   AND convert_from(stopped.proof_bytes,'UTF8')::JSONB#>'{observation,kernel_wait_echild}'='true'::JSONB
   AND stopped.proof_digest=encode(sha256(stopped.proof_bytes),'hex')
   AND convert_from(stopped.proof_bytes,'UTF8')::JSONB#>'{observation,guardian}'=convert_from(started.request_bytes,'UTF8')::JSONB#>'{observation,guardian}'
   AND convert_from(stopped.proof_bytes,'UTF8')::JSONB#>'{observation,pane}'=convert_from(started.request_bytes,'UTF8')::JSONB#>'{observation,pane}'
   AND profiled_result_origin_valid(attempt,owner_node,
    CASE p.proposal#>>'{negotiation,contract,id}'
     WHEN 'aq.admission.preparation-v2-contract' THEN 'phase_execution_v2'
     WHEN 'aq.admission.preparation-v2-contract.interactive-long-v1' THEN 'phase_execution_v2'
     WHEN 'aq.admission.preparation-registry-refresh-v1-contract' THEN 'phase_refresh_execution_v1'
     WHEN 'aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1' THEN 'phase_refresh_execution_v1'
     ELSE 'unsupported' END,
    g.receipt_digest,stopped.phase_ack_digest)
 ),FALSE)
$$;

CREATE OR REPLACE FUNCTION native_incarnation_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE pins JSONB;
BEGIN
 IF TG_OP='UPDATE' AND OLD.state->'native' IS NOT NULL AND OLD.state->'native'<>'null'::jsonb
    AND (NEW.state->'native' IS NULL OR NEW.state->'native'='null'::jsonb) THEN
  RAISE EXCEPTION 'native incarnation cannot become a fixture registration';
 END IF;
 IF NEW.state->'native' IS NOT NULL AND NEW.state->'native'<>'null'::jsonb THEN
  IF octet_length(NEW.state::text)>65536 OR NEW.state->'identity' IS DISTINCT FROM NEW.state#>'{native,request,identity}' THEN
   RAISE EXCEPTION 'invalid native incarnation metadata';
  END IF;
  IF TG_OP='INSERT' OR NEW.state IS DISTINCT FROM OLD.state THEN
   pins:=COALESCE(NULLIF(current_setting('aq.compute_stopped',true),'')::JSONB,'{}'::JSONB);
   IF EXISTS(SELECT 1 FROM execution_attempts a
    LEFT JOIN profiled_phase_execution_grants g USING(attempt_id)
    LEFT JOIN profiled_stopped_receipts s USING(attempt_id)
    LEFT JOIN profiled_v2_proposals p USING(attempt_id)
    WHERE a.node_id=NEW.node_id AND a.holds_task AND (
     NOT profiled_compute_stopped_valid(a.attempt_id,NEW.node_id)
     OR pins->a.attempt_id::TEXT IS DISTINCT FROM jsonb_build_object(
      'kind',CASE p.proposal#>>'{negotiation,contract,id}'
       WHEN 'aq.admission.preparation-v2-contract' THEN 'phase_execution_v2'
     WHEN 'aq.admission.preparation-v2-contract.interactive-long-v1' THEN 'phase_execution_v2'
       WHEN 'aq.admission.preparation-registry-refresh-v1-contract' THEN 'phase_refresh_execution_v1'
     WHEN 'aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1' THEN 'phase_refresh_execution_v1'
       ELSE 'unsupported' END,
      'execution',g.receipt_digest,'stopped_ack',s.phase_ack_digest))) THEN
    RAISE EXCEPTION 'execution claims fence native incarnation changes';
   END IF;
  END IF;
 END IF;
 RETURN NEW;
END $$;

-- Explicit long-policy admission identities. Same nominal terminal protocols;
-- preserve all original joins and old references, never substitute an old pin.
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
   AND ((origin_kind='phase_execution_v2' AND p.proposal#>'{negotiation,contract}' IN ('{"digest":"ff60400da09f3de166b7931bc386c89218cb76a6c9e5276dff37c5fce28ca3e5","id":"aq.admission.preparation-v2-contract","version":"1"}'::JSONB,'{"digest":"8e60c64136ecc6653bf9d1488b98832f02eb841bd3ceef462662a098dd13e599","id":"aq.admission.preparation-v2-contract.interactive-long-v1","version":"1"}'::JSONB) AND convert_from(stopped.phase_ack_bytes,'UTF8')::JSONB#>'{negotiation,contract}'='{"digest":"292c30eba95b8dd6f1d9a7bf75775bc5d0f9ecb54644047747f9593d538c0a4a","id":"aq.phase-terminal.contract","version":"1"}'::JSONB)
    OR (origin_kind='phase_refresh_execution_v1' AND p.proposal#>'{negotiation,contract}' IN ('{"digest":"e41c08e3c8200f84b75824bff779833c9b8e5279ac63d2d91af12d9879deef56","id":"aq.admission.preparation-registry-refresh-v1-contract","version":"1"}'::JSONB,'{"digest":"bd9abe1d9ef02970beb7946c007418de90a6066a146698ac0c075da1c1f2a28f","id":"aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1","version":"1"}'::JSONB) AND convert_from(stopped.phase_ack_bytes,'UTF8')::JSONB#>'{negotiation,contract}'='{"digest":"f9af55c49127a8cb93f03ef5bbb28313d13f94a221affa3e26d91d45bffd6c2d","id":"aq.phase-terminal.registry-refresh-v1.contract","version":"1"}'::JSONB))
 )
$$;

CREATE OR REPLACE FUNCTION profiled_close_expired_ungranted(p_node UUID,p_attempt UUID,p_now BIGINT,p_closure UUID,p_operation UUID)
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
     IF phase_body#>'{negotiation,contract}' IS NULL
       OR phase_body#>'{negotiation,contract}' NOT IN ('{"digest":"ff60400da09f3de166b7931bc386c89218cb76a6c9e5276dff37c5fce28ca3e5","id":"aq.admission.preparation-v2-contract","version":"1"}'::JSONB,'{"digest":"e41c08e3c8200f84b75824bff779833c9b8e5279ac63d2d91af12d9879deef56","id":"aq.admission.preparation-registry-refresh-v1-contract","version":"1"}'::JSONB,'{"digest":"8e60c64136ecc6653bf9d1488b98832f02eb841bd3ceef462662a098dd13e599","id":"aq.admission.preparation-v2-contract.interactive-long-v1","version":"1"}'::JSONB,'{"digest":"bd9abe1d9ef02970beb7946c007418de90a6066a146698ac0c075da1c1f2a28f","id":"aq.admission.preparation-registry-refresh-v1-contract.interactive-long-v1","version":"1"}'::JSONB)
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
 IF p_now<deadline THEN RETURN; END IF;
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
UPDATE meta SET value='41' WHERE key='state_format_version';
