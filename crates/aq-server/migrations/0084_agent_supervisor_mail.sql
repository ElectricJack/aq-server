-- bold-stone-51: an agent session may append mail to the singleton supervisor
-- conversation. Its proof is the agent-access v1 send receipt that queued it;
-- owner mail keeps the frozen human-supervisor-conversation-1 proof unchanged.
CREATE OR REPLACE FUNCTION central_global_mail_source_valid(p_message UUID) RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$
 SELECT (SELECT count(*)=1 AND (SELECT count(*) FROM mailbox_operations original WHERE original.response->>'message_id'=p_message::text
  AND original.response->>'contract'='human-supervisor-conversation-1' AND original.response->>'state'='queued')=1
 FROM mailbox_messages m JOIN global_supervisor_conversation g ON g.singleton
 JOIN mailbox_operations o ON o.response->>'message_id'=m.message_id::text
  AND o.response->>'contract'='human-supervisor-conversation-1' AND o.response->>'state'='queued'
 WHERE m.message_id=p_message AND m.mailbox_key=central_global_mailbox_key()
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
 FROM mailbox_messages m JOIN global_supervisor_conversation g ON g.singleton
 JOIN mailbox_operations o ON o.response->>'message_id'=m.message_id::text
  AND o.response#>>'{recipient,kind}'='global_supervisor'
 WHERE m.message_id=p_message AND m.mailbox_key=central_global_mailbox_key()
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
UPDATE meta SET value='84' WHERE key='state_format_version';
