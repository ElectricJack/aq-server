-- A physical generation remains one immutable turn. Keep UNIQUE(actor_id,generation).
-- Consumption releases reservation ownership, never the retained dispatch/mail hold.
ALTER TABLE central_supervisor_reservations DROP CONSTRAINT central_supervisor_reservations_state_check;
ALTER TABLE central_supervisor_reservations ADD CONSTRAINT central_supervisor_reservations_state_check
 CHECK(state IN ('awaiting_prepared_plan','cancelled_before_plan','consumed'));
ALTER TABLE central_supervisor_reservations DROP CONSTRAINT central_supervisor_reservations_check2;
ALTER TABLE central_supervisor_reservations ADD CONSTRAINT central_supervisor_reservations_check2
 CHECK((state='awaiting_prepared_plan' AND revision=1 AND cancel_operation_id IS NULL AND cancel_actor_revision IS NULL)
  OR (state='consumed' AND revision=2 AND cancel_operation_id IS NULL AND cancel_actor_revision IS NULL)
  OR (state='cancelled_before_plan' AND revision=2 AND cancel_operation_id IS NOT NULL
   AND cancel_operation_id<>'00000000-0000-0000-0000-000000000000'::uuid AND cancel_operation_id<>reserve_operation_id
   AND cancel_actor_revision IS NOT NULL AND cancel_actor_revision>admission_actor_revision));
CREATE FUNCTION central_supervisor_turn_complete(p_reservation UUID) RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
 SELECT EXISTS(SELECT 1 FROM central_supervisor_dispatches d WHERE d.reservation_id=p_reservation AND (
  EXISTS(SELECT 1 FROM central_supervisor_replies r WHERE r.dispatch_id=d.dispatch_id)
  OR EXISTS(SELECT 1 FROM central_session_generations g WHERE g.actor_id=d.actor_id AND g.generation=d.generation
   AND g.session_id=d.session_id AND g.state_bytes::jsonb#>>'{data,plan,dispatch_id}'=d.dispatch_id::text
   AND g.state_bytes::jsonb#>>'{data,plan,host_operation_id}'=d.host_operation_id::text
   AND g.state_bytes::jsonb#>>'{data,dispatch,kind}'='resolved'
   AND g.state_bytes::jsonb#>>'{data,dispatch,evidence,disposition}'='not_handed_off')))
$$;
CREATE OR REPLACE FUNCTION guard_central_reservation_actor() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN RAISE EXCEPTION 'central actor history cannot be removed'; END IF;
 -- Consumption only clears a proven completed turn; reservation operations and
 -- their revision/budget sequence remain unchanged.
 IF TG_OP='UPDATE' AND OLD.current_reservation IS NOT NULL AND NEW.current_reservation IS NULL
  AND (to_jsonb(NEW)-'current_reservation')=(to_jsonb(OLD)-'current_reservation')
  AND EXISTS(SELECT 1 FROM central_supervisor_reservations r WHERE r.actor_id=OLD.actor_id
   AND r.reservation_id=OLD.current_reservation AND r.state='consumed'
   AND central_supervisor_turn_complete(r.reservation_id)) THEN
  RETURN NEW;
 END IF;
 IF ROW(NEW.actor_id,NEW.conversation_id) IS DISTINCT FROM ROW(OLD.actor_id,OLD.conversation_id)
  OR NEW.revision<>OLD.revision+1 OR NEW.policy_revision NOT IN (OLD.policy_revision,OLD.policy_revision+1)
  OR NEW.retained_operations<>OLD.retained_operations+1 OR NEW.retained_bytes<=OLD.retained_bytes THEN
  RAISE EXCEPTION 'central actor identity/revision/budget transition refused';
 END IF;
 IF NEW.policy_revision=OLD.policy_revision AND ROW(NEW.policy_bytes,NEW.policy_digest,NEW.approved_by,NEW.approved_identity)
  IS DISTINCT FROM ROW(OLD.policy_bytes,OLD.policy_digest,OLD.approved_by,OLD.approved_identity) THEN
  RAISE EXCEPTION 'central policy update requires next revision';
 END IF;
 RETURN NEW;
END $$;
CREATE OR REPLACE FUNCTION guard_central_reservation_state() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN RAISE EXCEPTION 'central reservation history cannot be removed'; END IF;
 IF (to_jsonb(NEW)-ARRAY['state','revision','cancel_operation_id','cancel_actor_revision']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','revision','cancel_operation_id','cancel_actor_revision'])
  OR OLD.state<>'awaiting_prepared_plan' OR NEW.state NOT IN ('cancelled_before_plan','consumed') OR NEW.revision<>OLD.revision+1
  OR OLD.cancel_operation_id IS NOT NULL OR OLD.cancel_actor_revision IS NOT NULL THEN
  RAISE EXCEPTION 'central reservation is immutable except one pre-plan cancellation';
 END IF;
 IF NEW.state='consumed' AND NOT central_supervisor_turn_complete(NEW.reservation_id) THEN
  RAISE EXCEPTION 'central reservation consumption requires terminal turn evidence';
 END IF;
 IF NEW.state='cancelled_before_plan' AND EXISTS(SELECT 1 FROM central_supervisor_dispatches WHERE reservation_id=NEW.reservation_id) THEN
  RAISE EXCEPTION 'dispatched central reservation cannot be cancelled before plan';
 END IF;
 RETURN NEW;
END $$;
CREATE OR REPLACE FUNCTION check_central_reservation_consistency() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE a central_supervisor_actors; r central_supervisor_reservations; actual_count BIGINT; actual_bytes BIGINT; actual_digest TEXT; last_revision BIGINT; original_view JSONB; current_view JSONB;
BEGIN
 SELECT * INTO a FROM central_supervisor_actors WHERE actor_id=NEW.actor_id;
 IF (SELECT count(*) FROM central_supervisor_reservations WHERE actor_id=a.actor_id AND state='awaiting_prepared_plan')
  <>(CASE WHEN a.current_reservation IS NULL THEN 0 ELSE 1 END)
  OR (a.current_reservation IS NOT NULL AND NOT EXISTS(SELECT 1 FROM central_supervisor_reservations WHERE actor_id=a.actor_id AND reservation_id=a.current_reservation AND state='awaiting_prepared_plan')) THEN
  RAISE EXCEPTION 'central current reservation is inconsistent';
 END IF;
 IF TG_TABLE_NAME IN ('central_supervisor_actors','central_supervisor_operations') THEN
  SELECT count(*),COALESCE(sum(charged_bytes),0),COALESCE(max(actor_revision),0) INTO actual_count,actual_bytes,last_revision FROM central_supervisor_operations WHERE actor_id=a.actor_id;
  IF actual_count<>a.retained_operations OR actual_bytes<>a.retained_bytes OR last_revision<>a.revision THEN
   RAISE EXCEPTION 'central retained operation budget is inconsistent';
  END IF;
  RETURN NULL;
 END IF;
 SELECT * INTO r FROM central_supervisor_reservations WHERE reservation_id=NEW.reservation_id;
 SELECT count(*),COALESCE(sum(octet_length(m.body)),0),
  encode(sha256(convert_to('{"conversation_id":"'||a.conversation_id::text||'","messages":['||
   COALESCE(string_agg('{"cursor":'||b.cursor::text||',"message_id":"'||b.message_id::text||'","payload_digest":"'||b.payload_digest||'"}',',' ORDER BY b.ordinal),'')||
   '],"through_cursor":'||r.through_cursor::text||'}','UTF8')),'hex') INTO actual_count,actual_bytes,actual_digest
  FROM central_supervisor_mail_members b JOIN mailbox_messages m ON m.message_id=b.message_id
  WHERE b.reservation_id=r.reservation_id;
 IF actual_count<>r.message_count OR actual_bytes<>r.body_bytes OR actual_digest<>r.batch_digest
  OR EXISTS(SELECT 1 FROM (SELECT cursor,lag(cursor) OVER(ORDER BY ordinal) AS previous FROM central_supervisor_mail_members WHERE reservation_id=r.reservation_id) ordered WHERE previous>=cursor)
  OR EXISTS(SELECT 1 FROM central_supervisor_mail_members b JOIN mailbox_messages m ON m.message_id=b.message_id
   WHERE b.reservation_id=r.reservation_id AND (b.ordinal>=r.message_count OR b.actor_id<>r.actor_id OR b.mailbox_key<>m.mailbox_key OR b.cursor<>m.cursor OR b.payload_digest<>m.payload_digest
    OR b.mailbox_key<>central_global_mailbox_key() OR b.cursor>r.through_cursor OR NOT central_global_mail_source_valid(m.message_id)))
  THEN RAISE EXCEPTION 'central original reservation member proof is inconsistent'; END IF;
 SELECT jsonb_build_object('id',r.reservation_id::text,'actor_id',r.actor_id::text,'conversation_id',a.conversation_id::text,
  'revision',1,'admission_actor_revision',r.admission_actor_revision,'reserve_operation_id',r.reserve_operation_id::text,'cancel_operation_id',NULL,'policy_revision',r.policy_revision,
  'batch',jsonb_build_object('conversation_id',a.conversation_id::text,'through_cursor',r.through_cursor,
   'messages',jsonb_agg(jsonb_build_object('message_id',b.message_id::text,'cursor',b.cursor,'payload_digest',b.payload_digest) ORDER BY b.ordinal)),
  'batch_digest',r.batch_digest,'body_bytes',r.body_bytes,'state','awaiting_prepared_plan',
  'runtime','unavailable','prepared_plan',NULL,'session',NULL,'execution_history','unavailable','history_scope','reservation_only')
  INTO original_view FROM central_supervisor_mail_members b WHERE b.reservation_id=r.reservation_id;
 IF NOT EXISTS(SELECT 1 FROM central_supervisor_operations o WHERE o.actor_id=r.actor_id AND o.operation_id=r.reserve_operation_id AND o.kind='reserve'
  AND o.actor_revision=r.admission_actor_revision AND o.request_bytes::jsonb#>>'{request,reservation_id}'=r.reservation_id::text
  AND o.request_bytes::jsonb#>>'{request,expected_actor_revision}'=(r.admission_actor_revision-1)::text
  AND o.request_bytes::jsonb#>>'{request,expected_policy_revision}'=r.policy_revision::text
  AND o.request_bytes::jsonb#>>'{request,through_cursor}'=r.through_cursor::text
  AND o.result_bytes::jsonb->>'disposition'='reserved' AND o.result_bytes::jsonb->'reservation'=original_view
  AND o.result_bytes::jsonb->'reason'='null'::jsonb AND o.result_bytes::jsonb->'policy_revision'=to_jsonb(r.policy_revision)) THEN
  RAISE EXCEPTION 'central original reserve operation proof is inconsistent';
 END IF;
 IF r.state='consumed' AND NOT central_supervisor_turn_complete(r.reservation_id) THEN
  RAISE EXCEPTION 'central consumed reservation proof is inconsistent';
 END IF;
 IF r.state='cancelled_before_plan' THEN
  current_view=original_view||jsonb_build_object('revision',2,'state','cancelled_before_plan','cancel_operation_id',r.cancel_operation_id::text);
  IF NOT EXISTS(SELECT 1 FROM central_supervisor_operations o WHERE o.actor_id=r.actor_id AND o.operation_id=r.cancel_operation_id AND o.kind='cancel'
   AND o.actor_revision=r.cancel_actor_revision AND o.request_bytes::jsonb#>>'{request,reservation_id}'=r.reservation_id::text
   AND o.request_bytes::jsonb#>>'{request,expected_reservation_revision}'='1'
   AND o.result_bytes::jsonb->>'disposition'='cancelled_before_plan' AND o.result_bytes::jsonb->'reservation'=current_view
   AND o.result_bytes::jsonb->'reason'='null'::jsonb) THEN RAISE EXCEPTION 'central original cancel operation proof is inconsistent'; END IF;
 END IF;
 RETURN NULL;
END $$;

-- Both terminal paths take the actor lock before changing its reservation. This
-- is also the reservation owner's lock, so concurrent reserve/reply/report serialize.
CREATE FUNCTION consume_central_supervisor_turn() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE d central_supervisor_dispatches;
BEGIN
 IF TG_TABLE_NAME='central_supervisor_replies' THEN
  SELECT * INTO d FROM central_supervisor_dispatches WHERE dispatch_id=NEW.dispatch_id;
 ELSE
  SELECT * INTO d FROM central_supervisor_dispatches WHERE actor_id=NEW.actor_id AND generation=NEW.generation;
 END IF;
 IF d.dispatch_id IS NULL OR NOT central_supervisor_turn_complete(d.reservation_id) THEN RETURN NULL; END IF;
 PERFORM 1 FROM central_supervisor_actors WHERE actor_id=d.actor_id FOR UPDATE;
 UPDATE central_supervisor_reservations SET state='consumed',revision=2
  WHERE actor_id=d.actor_id AND reservation_id=d.reservation_id AND state='awaiting_prepared_plan';
 UPDATE central_supervisor_actors SET current_reservation=NULL
  WHERE actor_id=d.actor_id AND current_reservation=d.reservation_id;
 RETURN NULL;
END $$;
CREATE TRIGGER central_supervisor_reply_consumption AFTER INSERT ON central_supervisor_replies
 FOR EACH ROW EXECUTE FUNCTION consume_central_supervisor_turn();
CREATE TRIGGER central_supervisor_delivery_consumption AFTER INSERT OR UPDATE ON central_session_generations
 FOR EACH ROW EXECUTE FUNCTION consume_central_supervisor_turn();
-- Upgrade already recorded terminal turns without changing original receipts or holds.
UPDATE central_supervisor_reservations SET state='consumed',revision=2
 WHERE state='awaiting_prepared_plan' AND central_supervisor_turn_complete(reservation_id);
UPDATE central_supervisor_actors a SET current_reservation=NULL WHERE EXISTS(
 SELECT 1 FROM central_supervisor_reservations r WHERE r.actor_id=a.actor_id
 AND r.reservation_id=a.current_reservation AND r.state='consumed');
INSERT INTO meta(key,value) VALUES('state_format_version','49') ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value;
