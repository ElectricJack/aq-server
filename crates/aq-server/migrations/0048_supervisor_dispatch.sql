-- F1b: one supervisor turn per reservation, the reply association, and the
-- generation-bound supervisor credential (sha256 digest only, never plaintext).
CREATE TABLE central_supervisor_dispatches (
 dispatch_id UUID PRIMARY KEY CHECK(dispatch_id<>'00000000-0000-0000-0000-000000000000'::uuid),
 host_operation_id UUID NOT NULL UNIQUE CHECK(host_operation_id<>'00000000-0000-0000-0000-000000000000'::uuid AND host_operation_id<>dispatch_id),
 actor_id UUID NOT NULL REFERENCES central_supervisor_actors(actor_id),
 reservation_id UUID NOT NULL,
 generation BIGINT NOT NULL CHECK(generation BETWEEN 1 AND 1024),
 session_id UUID NOT NULL CHECK(session_id<>'00000000-0000-0000-0000-000000000000'::uuid),
 batch_digest TEXT NOT NULL CHECK(batch_digest ~ '^[0-9a-f]{64}$'),
 input_digest TEXT NOT NULL CHECK(input_digest ~ '^[0-9a-f]{64}$'),
 input_bytes BYTEA NOT NULL CHECK(octet_length(input_bytes) BETWEEN 1 AND 1048576 AND input_digest=encode(sha256(input_bytes),'hex')),
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>0),
 UNIQUE(actor_id,reservation_id), UNIQUE(actor_id,generation),
 FOREIGN KEY(actor_id,reservation_id) REFERENCES central_supervisor_reservations(actor_id,reservation_id)
);
CREATE TABLE central_supervisor_replies (
 reply_message_id UUID PRIMARY KEY REFERENCES mailbox_messages(message_id),
 reply_operation_id UUID NOT NULL CHECK(reply_operation_id<>reply_message_id AND reply_operation_id<>'00000000-0000-0000-0000-000000000000'::uuid),
 dispatch_id UUID NOT NULL REFERENCES central_supervisor_dispatches(dispatch_id),
 body_digest TEXT NOT NULL CHECK(body_digest ~ '^[0-9a-f]{64}$'),
 association JSONB NOT NULL CHECK(jsonb_typeof(association)='object' AND association->>'reply_message_id'=reply_message_id::text)
);
CREATE INDEX central_supervisor_reply_dispatch ON central_supervisor_replies(dispatch_id,reply_message_id);
CREATE TABLE central_supervisor_credentials (
 token_digest TEXT PRIMARY KEY CHECK(token_digest ~ '^[0-9a-f]{64}$'),
 actor_id UUID NOT NULL REFERENCES central_session_heads(actor_id),
 generation BIGINT NOT NULL CHECK(generation BETWEEN 1 AND 1024),
 session_id UUID NOT NULL,
 scopes TEXT[] NOT NULL CHECK(cardinality(scopes) BETWEEN 1 AND 8),
 issued_at_ms BIGINT NOT NULL CHECK(issued_at_ms>0),
 expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>issued_at_ms),
 revoked_at_ms BIGINT CHECK(revoked_at_ms IS NULL OR revoked_at_ms>0),
 FOREIGN KEY(actor_id,generation) REFERENCES central_session_generations(actor_id,generation)
);
CREATE UNIQUE INDEX central_supervisor_live_credential ON central_supervisor_credentials(actor_id)
 WHERE revoked_at_ms IS NULL;
CREATE FUNCTION retain_central_supervisor_turn() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 RAISE EXCEPTION 'central supervisor dispatch and reply evidence is immutable';
END $$;
CREATE TRIGGER central_supervisor_dispatch_immutable BEFORE UPDATE OR DELETE ON central_supervisor_dispatches
 FOR EACH ROW EXECUTE FUNCTION retain_central_supervisor_turn();
CREATE TRIGGER central_supervisor_dispatch_no_truncate BEFORE TRUNCATE ON central_supervisor_dispatches
 FOR EACH STATEMENT EXECUTE FUNCTION retain_central_supervisor_turn();
CREATE TRIGGER central_supervisor_reply_immutable BEFORE UPDATE OR DELETE ON central_supervisor_replies
 FOR EACH ROW EXECUTE FUNCTION retain_central_supervisor_turn();
CREATE TRIGGER central_supervisor_reply_no_truncate BEFORE TRUNCATE ON central_supervisor_replies
 FOR EACH STATEMENT EXECUTE FUNCTION retain_central_supervisor_turn();
-- Credentials may only be revoked, once.
CREATE FUNCTION guard_central_supervisor_credential() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP <> 'UPDATE' OR OLD.revoked_at_ms IS NOT NULL OR NEW.revoked_at_ms IS NULL
  OR (to_jsonb(NEW) - 'revoked_at_ms') <> (to_jsonb(OLD) - 'revoked_at_ms') THEN
  RAISE EXCEPTION 'central supervisor credentials can only be revoked';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER central_supervisor_credential_guard BEFORE UPDATE OR DELETE ON central_supervisor_credentials
 FOR EACH ROW EXECUTE FUNCTION guard_central_supervisor_credential();
CREATE TRIGGER central_supervisor_credential_no_truncate BEFORE TRUNCATE ON central_supervisor_credentials
 FOR EACH STATEMENT EXECUTE FUNCTION retain_central_supervisor_turn();
-- Host handoff proof is a journaled fact beside the F1a host facts.
ALTER TABLE central_host_facts DROP CONSTRAINT central_host_facts_kind_check;
ALTER TABLE central_host_facts ADD CONSTRAINT central_host_facts_kind_check
 CHECK(kind IN ('intent','guardian','start','running','stopped','lost','handoff'));
INSERT INTO meta(key,value) VALUES('state_format_version','48') ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value;
