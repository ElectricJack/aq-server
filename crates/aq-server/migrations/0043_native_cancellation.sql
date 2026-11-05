-- Durable authority cancellation is independent of physical writer stop.
CREATE TABLE profiled_cancellations (
 attempt_id UUID PRIMARY KEY REFERENCES profiled_authority_hot(attempt_id),
 cancellation_id UUID NOT NULL UNIQUE CHECK(cancellation_id<>'00000000-0000-0000-0000-000000000000'::UUID),
 binding JSONB NOT NULL CHECK(octet_length(binding::TEXT)<=65536),
 reason TEXT NOT NULL CHECK(octet_length(reason) BETWEEN 1 AND 4096),
 authority_revision BIGINT NOT NULL CHECK(authority_revision>0),
 recorded_at_ms BIGINT NOT NULL CHECK(recorded_at_ms>0),
 CHECK(binding->>'attempt_id'=attempt_id::TEXT)
);
CREATE TABLE profiled_cancel_operations (
 operation_id UUID PRIMARY KEY,
 attempt_id UUID NOT NULL REFERENCES profiled_authority_hot(attempt_id),
 request JSONB NOT NULL CHECK(octet_length(request::TEXT)<=8192),
 response JSONB NOT NULL CHECK(octet_length(response::TEXT)<=65536)
);
CREATE TABLE profiled_cancelled_settlements (
 attempt_id UUID PRIMARY KEY REFERENCES profiled_cancellations(attempt_id),
 cancellation_id UUID NOT NULL REFERENCES profiled_cancellations(cancellation_id),
 stop_receipt_digest TEXT NOT NULL CHECK(stop_receipt_digest ~ '^[0-9a-f]{64}$'),
 recorded_at_ms BIGINT NOT NULL CHECK(recorded_at_ms>0)
);
CREATE TRIGGER profiled_cancellations_immutable BEFORE UPDATE OR DELETE ON profiled_cancellations
 FOR EACH ROW EXECUTE FUNCTION profiled_sealed_row_guard();
CREATE TRIGGER profiled_cancel_operations_immutable BEFORE UPDATE OR DELETE ON profiled_cancel_operations
 FOR EACH ROW EXECUTE FUNCTION profiled_sealed_row_guard();
CREATE TRIGGER profiled_cancelled_settlements_immutable BEFORE UPDATE OR DELETE ON profiled_cancelled_settlements
 FOR EACH ROW EXECUTE FUNCTION profiled_sealed_row_guard();
-- A cancelled disposition is possible only against the exact actual stopped
-- owner row and its hot projection. It never releases source/effect claims.
CREATE FUNCTION profiled_cancelled_settlement_guard() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF NOT EXISTS(SELECT 1 FROM profiled_cancellations c
 JOIN profiled_authority_hot h USING(attempt_id)
 JOIN profiled_stopped_receipts s USING(attempt_id)
 WHERE c.attempt_id=NEW.attempt_id AND c.cancellation_id=NEW.cancellation_id
 AND h.state->>'cancellation_id'=c.cancellation_id::TEXT
 AND h.state#>>'{stop,kind}'='stopped'
 AND h.state#>>'{stop,receipt,digest}'=NEW.stop_receipt_digest
 AND encode(sha256(s.receipt_bytes),'hex')=NEW.stop_receipt_digest
 AND (convert_from(s.request_bytes,'UTF8')::JSONB)#>'{evidence,binding}'=c.binding)
 THEN RAISE EXCEPTION 'actual stopped cancellation proof required' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_cancelled_settlement_guard BEFORE INSERT ON profiled_cancelled_settlements
 FOR EACH ROW EXECUTE FUNCTION profiled_cancelled_settlement_guard();
ALTER TABLE profiled_never_granted_closures DROP CONSTRAINT profiled_never_granted_closures_reason_check;
ALTER TABLE profiled_never_granted_closures ADD CONSTRAINT profiled_never_granted_closures_reason_check
 CHECK(reason IN ('offer_expired','preparation_expired','cancelled'));
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
-- Raw closure inserts retain the same durable cancellation join as the owner.
CREATE FUNCTION profiled_cancelled_absence_guard() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.reason='cancelled' AND NOT EXISTS(SELECT 1 FROM profiled_cancellations c JOIN profiled_authority_hot h USING(attempt_id)
 WHERE c.attempt_id=NEW.attempt_id AND c.cancellation_id=NEW.operation_id
 AND h.state->>'cancellation_id'=c.cancellation_id::TEXT AND c.binding=NEW.binding
 AND c.recorded_at_ms<=NEW.closed_at_ms)
 THEN RAISE EXCEPTION 'actual cancellation absence proof required' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER profiled_cancelled_absence_guard BEFORE INSERT ON profiled_never_granted_closures
 FOR EACH ROW EXECUTE FUNCTION profiled_cancelled_absence_guard();
-- Preserve the expiry entrypoint and its strict deadline semantics.
CREATE OR REPLACE FUNCTION profiled_close_expired_ungranted(p_node UUID,p_attempt UUID,p_now BIGINT,p_closure UUID,p_operation UUID)
RETURNS TABLE(body_bytes BYTEA,body_digest TEXT,changed BOOLEAN) LANGUAGE SQL AS $$
 SELECT * FROM profiled_close_ungranted(p_node,p_attempt,p_now,p_closure,p_operation,FALSE)
$$;
UPDATE meta SET value='43' WHERE key='state_format_version';
