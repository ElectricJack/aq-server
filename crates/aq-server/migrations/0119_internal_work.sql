-- Internal work (spec 0012 §4): model requests made by AQ itself, kept as
-- server records outside the task graph and outside project capacity.
CREATE TABLE internal_tasks (
    id UUID PRIMARY KEY,
    purpose TEXT NOT NULL,
    -- The first version has one kind; the column exists so a second kind
    -- (a repository to read) does not change the first.
    workspace_kind TEXT NOT NULL DEFAULT 'none' CHECK(workspace_kind IN ('none')),
    request JSONB NOT NULL,
    -- Resolved profile entries in fallback order; candidate_index is the one
    -- that holds or last held the task.
    candidates JSONB NOT NULL CHECK(jsonb_typeof(candidates)='array'),
    candidate_index INT NOT NULL DEFAULT 0 CHECK(candidate_index>=0),
    status TEXT NOT NULL CHECK(status IN ('queued','running','answered','failed')),
    blocking BOOLEAN NOT NULL,
    runner TEXT,
    claimed_at_ms BIGINT,
    deadline_at_ms BIGINT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    attempts JSONB NOT NULL DEFAULT '[]' CHECK(jsonb_typeof(attempts)='array'),
    answer JSONB,
    usage JSONB,
    reason TEXT,
    detail TEXT,
    affected_kind TEXT NOT NULL,
    affected_id TEXT NOT NULL,
    project_id UUID,
    escalate BOOLEAN NOT NULL,
    escalation TEXT NOT NULL DEFAULT 'none' CHECK(escalation IN ('none','supervisor','owner','resolved')),
    supervisor_attempts INT NOT NULL DEFAULT 0,
    diagnosis TEXT,
    retry_of UUID,
    -- When the run last moved to a supervisor or the owner (spec 0012 §8).
    escalated_at_ms BIGINT
);
CREATE INDEX internal_tasks_queue ON internal_tasks(status, blocking DESC, created_at_ms);
CREATE INDEX internal_tasks_affected ON internal_tasks(affected_kind, affected_id);
-- The escalation step (spec 0012 §8) runs every poll; these keep it off the
-- whole table as failed runs accumulate.
CREATE INDEX internal_tasks_unescalated ON internal_tasks(id)
    WHERE status='failed' AND escalation='none' AND (escalate OR retry_of IS NOT NULL);
CREATE INDEX internal_tasks_failures ON internal_tasks(finished_at_ms) WHERE status='failed';
CREATE INDEX internal_tasks_escalated ON internal_tasks(purpose, escalated_at_ms)
    WHERE escalated_at_ms IS NOT NULL;
CREATE INDEX internal_tasks_retries ON internal_tasks(retry_of) WHERE retry_of IS NOT NULL;
-- Which intelligence profile internal work uses: one default and per-purpose
-- overrides (spec 0012 §5).
CREATE TABLE internal_work_settings (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK(singleton),
    revision BIGINT NOT NULL DEFAULT 0 CHECK(revision >= 0),
    document JSONB NOT NULL CHECK(jsonb_typeof(document) = 'object')
);
INSERT INTO internal_work_settings(singleton, document) VALUES(TRUE,
    '{"default_profile_id":null,"overrides":{}}');
CREATE TABLE internal_work_settings_operations (
    actor_id UUID NOT NULL REFERENCES users(id),
    operation_id UUID NOT NULL,
    payload_digest TEXT NOT NULL,
    response JSONB NOT NULL,
    PRIMARY KEY(actor_id,operation_id)
);
-- A failed internal run tells a supervisor in its own conversation (spec 0012
-- §8). The sender is the fixed system component, as for the stall ladder's
-- nudges; its receipt proves the message like an owner's or a session's does.
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
 OR (SELECT count(*)=1
 FROM mailbox_messages m JOIN supervisor_conversation_ids g ON g.conversation_id=m.thread_id
 JOIN mailbox_operations o ON o.response->>'message_id'=m.message_id::text
  AND o.response->>'contract'='internal-work-supervisor-notice-1'
 WHERE m.message_id=p_message AND m.mailbox_key=encode(sha256(convert_to('{"conversation_id":"'||g.conversation_id::text||'","kind":"global_supervisor"}','UTF8')),'hex')
 AND m.recipient=jsonb_build_object('kind','global_supervisor','conversation_id',g.conversation_id::text)
 AND m.thread_id=g.conversation_id AND m.created_at_ms>0
 AND m.sender='{"kind":"system","component":"internal_work"}'::jsonb
 AND m.payload_digest ~ '^[0-9a-f]{64}$'
 AND o.sender_key=encode(sha256(convert_to('{"component":"internal_work","kind":"system"}','UTF8')),'hex')
 AND o.payload_digest=m.payload_digest AND o.operation_id<>m.message_id
 AND o.response->>'operation_id'=o.operation_id::text
 AND o.response->>'thread_id'=g.conversation_id::text
 AND o.response->>'cursor'=m.cursor::text
 AND o.response->>'disposition'='queued')
$$;
UPDATE meta SET value='119' WHERE key='state_format_version';
