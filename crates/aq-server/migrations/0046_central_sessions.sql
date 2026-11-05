-- Dormant central session persistence. No route, host or credential activation.
CREATE TABLE central_session_heads (
 actor_id UUID PRIMARY KEY CHECK(actor_id <> '00000000-0000-0000-0000-000000000000'),
 conversation_id UUID NOT NULL UNIQUE CHECK(conversation_id <> '00000000-0000-0000-0000-000000000000'),
 authority_id UUID NOT NULL CHECK(authority_id <> '00000000-0000-0000-0000-000000000000'),
 current_generation BIGINT NOT NULL CHECK(current_generation BETWEEN 0 AND 1024)
);
CREATE TABLE central_session_generations (
 actor_id UUID NOT NULL REFERENCES central_session_heads(actor_id),
 generation BIGINT NOT NULL CHECK(generation BETWEEN 1 AND 1024),
 session_id UUID NOT NULL UNIQUE CHECK(session_id <> '00000000-0000-0000-0000-000000000000'),
 revision BIGINT NOT NULL CHECK(revision > 0),
 state_bytes TEXT NOT NULL CHECK(octet_length(state_bytes) BETWEEN 1 AND 65536),
 state_digest TEXT NOT NULL CHECK(state_digest=encode(sha256(convert_to(state_bytes,'UTF8')),'hex')),
 PRIMARY KEY(actor_id,generation),
 CHECK ((state_bytes::jsonb->>'document' = 'state'
   AND state_bytes::jsonb#>>'{data,plan,subject,actor_id}' = actor_id::text
   AND state_bytes::jsonb#>>'{data,plan,generation}' = generation::text
   AND state_bytes::jsonb#>>'{data,plan,session,kind}' = 'central'
   AND state_bytes::jsonb#>>'{data,plan,session,session_id}' = session_id::text
   AND state_bytes::jsonb#>>'{data,plan,session,generation}' = generation::text) IS TRUE)
);
CREATE FUNCTION guard_central_session_head() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP <> 'UPDATE' THEN
  RAISE EXCEPTION 'central session history cannot be deleted';
 END IF;
 IF NEW.actor_id <> OLD.actor_id OR NEW.conversation_id <> OLD.conversation_id
  OR NEW.authority_id <> OLD.authority_id
  OR NEW.current_generation NOT IN (OLD.current_generation, OLD.current_generation + 1) THEN
  RAISE EXCEPTION 'central session identity is immutable and generations are consecutive';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER central_session_head_guard BEFORE UPDATE OR DELETE ON central_session_heads
 FOR EACH ROW EXECUTE FUNCTION guard_central_session_head();
CREATE TRIGGER central_session_head_truncate_guard BEFORE TRUNCATE ON central_session_heads
 FOR EACH STATEMENT EXECUTE FUNCTION guard_central_session_head();
CREATE FUNCTION guard_central_session_generation() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE current BIGINT;
BEGIN
 IF TG_OP NOT IN ('INSERT','UPDATE') THEN
  RAISE EXCEPTION 'central session history cannot be deleted';
 END IF;
 SELECT current_generation INTO current FROM central_session_heads WHERE actor_id=NEW.actor_id FOR UPDATE;
 IF NEW.generation <> current THEN
  RAISE EXCEPTION 'only the current central generation is writable';
 END IF;
 IF TG_OP = 'INSERT' AND NEW.revision <> 1 THEN
  RAISE EXCEPTION 'central generation starts at revision one';
 END IF;
 IF TG_OP = 'UPDATE' AND (NEW.actor_id <> OLD.actor_id OR NEW.generation <> OLD.generation
  OR NEW.session_id <> OLD.session_id OR NEW.revision <> OLD.revision + 1
  OR NEW.state_bytes::jsonb#>'{data,plan}' <> OLD.state_bytes::jsonb#>'{data,plan}'
  OR NEW.state_bytes::jsonb#>'{data,admitted}' <> OLD.state_bytes::jsonb#>'{data,admitted}'
  OR NEW.state_bytes::jsonb#>'{data,original_deadline_boot_ms}' <> OLD.state_bytes::jsonb#>'{data,original_deadline_boot_ms}') THEN
  RAISE EXCEPTION 'central generation identity/deadline is immutable';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER central_session_generation_guard BEFORE INSERT OR UPDATE OR DELETE ON central_session_generations
 FOR EACH ROW EXECUTE FUNCTION guard_central_session_generation();
CREATE TRIGGER central_session_generation_truncate_guard BEFORE TRUNCATE ON central_session_generations
 FOR EACH STATEMENT EXECUTE FUNCTION guard_central_session_generation();
INSERT INTO meta(key,value) VALUES('state_format_version','46') ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value;
