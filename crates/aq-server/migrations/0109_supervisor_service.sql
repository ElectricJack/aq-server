-- One central conversation per scope; keep the singleton global identity/counters.
CREATE TABLE supervisor_conversation_ids (
 conversation_id UUID PRIMARY KEY CHECK(conversation_id<>'00000000-0000-0000-0000-000000000000'),
 project_id UUID UNIQUE REFERENCES project_execution_bindings(project_id)
);
CREATE UNIQUE INDEX supervisor_one_global ON supervisor_conversation_ids((project_id IS NULL)) WHERE project_id IS NULL;
INSERT INTO supervisor_conversation_ids SELECT conversation_id,NULL FROM global_supervisor_conversation;
CREATE TABLE project_supervisor_conversations (
 conversation_id UUID PRIMARY KEY REFERENCES supervisor_conversation_ids(conversation_id),
 project_id UUID NOT NULL UNIQUE REFERENCES project_execution_bindings(project_id),
 label TEXT NOT NULL CHECK(length(label) BETWEEN 1 AND 256),
 revision BIGINT NOT NULL DEFAULT 1 CHECK(revision=1),
 retained_operations BIGINT NOT NULL DEFAULT 0 CHECK(retained_operations BETWEEN 0 AND 4096),
 retained_bytes BIGINT NOT NULL DEFAULT 0 CHECK(retained_bytes BETWEEN 0 AND 67108864)
);
CREATE TABLE supervisor_controls (
 conversation_id UUID PRIMARY KEY REFERENCES supervisor_conversation_ids(conversation_id),
 actor_id UUID NOT NULL UNIQUE DEFAULT gen_random_uuid(),
 enabled BOOLEAN NOT NULL DEFAULT FALSE,
 revision BIGINT NOT NULL DEFAULT 0 CHECK(revision>=0),
 profile JSONB,
 approved_by UUID REFERENCES users(id),
 updated_at_ms BIGINT NOT NULL DEFAULT 0,
 CHECK(NOT enabled OR (profile IS NOT NULL AND approved_by IS NOT NULL))
);
CREATE TABLE supervisor_control_operations (
 operation_id UUID PRIMARY KEY,
 actor_id UUID NOT NULL REFERENCES users(id),
 request JSONB NOT NULL,
 receipt JSONB NOT NULL,
 created_at_ms BIGINT NOT NULL
);
CREATE TABLE supervisor_service_reports (
 conversation_id UUID PRIMARY KEY REFERENCES supervisor_controls(conversation_id),
 instance_id UUID NOT NULL,
 observed_at_ms BIGINT NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN ('idle','working','waiting','stopped','stopping','failed','uncertain')),
 stage TEXT NOT NULL CHECK(length(stage)<=256),
 generation BIGINT,
 session_id UUID,
 last_error TEXT CHECK(length(last_error)<=512)
);
CREATE FUNCTION supervisor_scope_identity_guard() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP<>'UPDATE' THEN RAISE EXCEPTION 'supervisor identity/history is retained'; END IF;
 IF TG_TABLE_NAME='project_supervisor_conversations' THEN
  IF ROW(NEW.conversation_id,NEW.project_id,NEW.revision) IS DISTINCT FROM ROW(OLD.conversation_id,OLD.project_id,OLD.revision)
   OR NEW.retained_operations<OLD.retained_operations OR NEW.retained_bytes<OLD.retained_bytes THEN RAISE EXCEPTION 'supervisor identity/history is retained'; END IF;
 ELSE RAISE EXCEPTION 'supervisor scope is immutable'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER supervisor_scope_immutable BEFORE UPDATE OR DELETE ON supervisor_conversation_ids FOR EACH ROW EXECUTE FUNCTION supervisor_scope_identity_guard();
CREATE TRIGGER project_supervisor_history BEFORE UPDATE OR DELETE ON project_supervisor_conversations FOR EACH ROW EXECUTE FUNCTION supervisor_scope_identity_guard();
CREATE TRIGGER supervisor_scope_no_truncate BEFORE TRUNCATE ON supervisor_conversation_ids FOR EACH STATEMENT EXECUTE FUNCTION supervisor_scope_identity_guard();
CREATE TRIGGER project_supervisor_no_truncate BEFORE TRUNCATE ON project_supervisor_conversations FOR EACH STATEMENT EXECUTE FUNCTION supervisor_scope_identity_guard();
CREATE FUNCTION ensure_project_supervisor_scope() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE cid UUID;
BEGIN
 IF EXISTS(SELECT 1 FROM supervisor_conversation_ids WHERE project_id=NEW.project_id) THEN RETURN NEW; END IF;
 cid=gen_random_uuid();
 INSERT INTO supervisor_conversation_ids VALUES(cid,NEW.project_id);
 INSERT INTO project_supervisor_conversations(conversation_id,project_id,label) VALUES(cid,NEW.project_id,left(COALESCE(NEW.binding->>'name','Project')||' supervisor',256));
 INSERT INTO supervisor_controls(conversation_id) VALUES(cid);
 RETURN NEW;
END $$;
CREATE TRIGGER project_supervisor_scope AFTER INSERT ON project_execution_bindings FOR EACH ROW EXECUTE FUNCTION ensure_project_supervisor_scope();
DO $$
DECLARE p RECORD; cid UUID;
BEGIN
 FOR p IN SELECT project_id,binding FROM project_execution_bindings LOOP
  cid=gen_random_uuid();
  INSERT INTO supervisor_conversation_ids VALUES(cid,p.project_id);
  INSERT INTO project_supervisor_conversations(conversation_id,project_id,label) VALUES(cid,p.project_id,left(COALESCE(p.binding->>'name','Project')||' supervisor',256));
 END LOOP;
END $$;
INSERT INTO supervisor_controls(conversation_id,actor_id) SELECT i.conversation_id,COALESCE(a.actor_id,gen_random_uuid()) FROM supervisor_conversation_ids i LEFT JOIN central_supervisor_actors a USING(conversation_id) ON CONFLICT DO NOTHING;
SET CONSTRAINTS ALL IMMEDIATE;
ALTER TABLE central_supervisor_actors DROP CONSTRAINT central_supervisor_actors_conversation_id_fkey;
ALTER TABLE central_supervisor_actors ADD CONSTRAINT central_supervisor_actors_conversation_id_fkey FOREIGN KEY(conversation_id) REFERENCES supervisor_conversation_ids(conversation_id);

-- Preserve original mail proofs while allowing the bound project conversations.
CREATE OR REPLACE FUNCTION central_global_mail_source_valid(p_message UUID) RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
 SELECT (SELECT count(*)=1 AND (SELECT count(*) FROM mailbox_operations original WHERE original.response->>'message_id'=p_message::text
  AND original.response->>'contract'='human-supervisor-conversation-1' AND original.response->>'state'='queued')=1
 FROM mailbox_messages m JOIN supervisor_conversation_ids g ON g.conversation_id=m.thread_id
 JOIN mailbox_operations o ON o.response->>'message_id'=m.message_id::text
  AND o.response->>'contract'='human-supervisor-conversation-1' AND o.response->>'state'='queued'
 WHERE m.message_id=p_message AND m.mailbox_key=encode(sha256(convert_to('{"conversation_id":"'||g.conversation_id::text||'","kind":"global_supervisor"}','UTF8')),'hex')
 AND m.recipient=jsonb_build_object('kind','global_supervisor','conversation_id',g.conversation_id::text)
 AND m.thread_id=g.conversation_id AND m.created_at_ms>0
 AND m.sender=jsonb_build_object('kind','owner','user_id',m.sender->>'user_id')
 AND m.sender->>'user_id' ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
 AND m.sender->>'user_id'<>'00000000-0000-0000-0000-000000000000'
 AND m.payload_digest ~ '^[0-9a-f]{64}$'
 AND o.sender_key=encode(sha256(convert_to('{"kind":"owner","user_id":"'||(m.sender->>'user_id')||'"}','UTF8')),'hex')
 AND o.payload_digest=m.payload_digest AND o.operation_id<>m.message_id
 AND o.response->>'operation_id'=o.operation_id::text
 AND o.response->>'actor_id'=m.sender->>'user_id'
 AND o.response#>>'{conversation,id}'=g.conversation_id::text
 AND o.response#>>'{conversation,revision}'='1'
 AND o.response->>'cursor'=m.cursor::text
 AND o.response->>'body_digest'='sha256:'||encode(sha256(convert_to(m.body,'UTF8')),'hex'))
 OR (SELECT count(*)=1
 FROM mailbox_messages m JOIN supervisor_conversation_ids g ON g.conversation_id=m.thread_id
 JOIN mailbox_operations o ON o.response->>'message_id'=m.message_id::text
  AND o.response#>>'{recipient,kind}'='global_supervisor'
 WHERE m.message_id=p_message AND m.mailbox_key=encode(sha256(convert_to('{"conversation_id":"'||g.conversation_id::text||'","kind":"global_supervisor"}','UTF8')),'hex')
 AND m.recipient=jsonb_build_object('kind','global_supervisor','conversation_id',g.conversation_id::text)
 AND m.thread_id=g.conversation_id AND m.created_at_ms>0
 AND m.sender=jsonb_build_object('kind','session','binding',m.sender->'binding')
 AND jsonb_typeof(m.sender->'binding')='object'
 AND m.payload_digest ~ '^[0-9a-f]{64}$'
 AND o.sender_key ~ '^[0-9a-f]{64}$'
 AND o.payload_digest=m.payload_digest
 AND o.response->'recipient'=m.recipient
 AND o.response->>'api_version'='1'
 AND o.response->>'operation_id'=o.operation_id::text
 AND o.response->>'thread_id'=g.conversation_id::text
 AND o.response->>'cursor'=m.cursor::text
 AND o.response->>'disposition'='queued')
$$;

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
    OR b.mailbox_key<>encode(sha256(convert_to('{"conversation_id":"'||a.conversation_id::text||'","kind":"global_supervisor"}','UTF8')),'hex') OR b.cursor>r.through_cursor OR NOT central_global_mail_source_valid(m.message_id)))
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
UPDATE meta SET value='109' WHERE key='state_format_version';
