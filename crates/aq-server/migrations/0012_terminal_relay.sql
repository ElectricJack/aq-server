-- AQ21: exact profiled terminal registration and encrypted bounded delivery state.
-- Preceded by AQ24 migration0011. No terminal text/input is stored in audit JSON.
CREATE TABLE session_inspection_acl (
 session_key TEXT NOT NULL CHECK(session_key~'^[0-9a-f]{64}$'),
 session_identity JSONB NOT NULL CHECK(octet_length(session_identity::text)<=4096),
 principal_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
 revision BIGINT NOT NULL CHECK(revision>0),
 can_inspect BOOLEAN NOT NULL,
 expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>0),
 PRIMARY KEY(session_key,principal_id)
);
CREATE FUNCTION session_inspection_binding_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF (NEW.session_key,NEW.session_identity,NEW.principal_id) IS DISTINCT FROM
    (OLD.session_key,OLD.session_identity,OLD.principal_id) OR NEW.revision<=OLD.revision
 THEN RAISE EXCEPTION 'immutable session inspection binding or stale revision' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER session_inspection_binding_immutable BEFORE UPDATE ON session_inspection_acl
 FOR EACH ROW EXECUTE FUNCTION session_inspection_binding_guard();
CREATE TABLE terminal_sessions (
 terminal_id UUID PRIMARY KEY,
 attempt_id UUID NOT NULL UNIQUE REFERENCES execution_attempts(attempt_id),
 node_id UUID NOT NULL REFERENCES nodes(id),
 boot_id UUID NOT NULL,
 session_id UUID NOT NULL,
 assignment_generation BIGINT NOT NULL CHECK(assignment_generation>0),
 terminal_epoch BIGINT NOT NULL CHECK(terminal_epoch>0),
 identity JSONB NOT NULL CHECK(octet_length(identity::text)<=4096),
 authority_revision BIGINT NOT NULL CHECK(authority_revision>0),
 execution_spec_digest TEXT NOT NULL CHECK(execution_spec_digest~'^[0-9a-f]{64}$'),
 launch_manifest_digest TEXT NOT NULL CHECK(launch_manifest_digest~'^[0-9a-f]{64}$'),
 input_mode TEXT NOT NULL CHECK(input_mode IN('finite_v1','epoch_v1')),
 ledger_revision BIGINT NOT NULL CHECK(ledger_revision>=0),
 runtime_id UUID NOT NULL,
 protection_version INTEGER NOT NULL CHECK(protection_version=1),
 key_id TEXT NOT NULL CHECK(octet_length(key_id) BETWEEN 1 AND 64),
 nonce BYTEA NOT NULL CHECK(octet_length(nonce)=12),
 ciphertext BYTEA NOT NULL CHECK(octet_length(ciphertext) BETWEEN 16 AND 1048592),
 controller_generation BIGINT NOT NULL CHECK(controller_generation>=0),
 controller_principal UUID,
 pending_bytes INTEGER NOT NULL CHECK(pending_bytes BETWEEN 0 AND 32768),
 operation_count INTEGER NOT NULL CHECK(operation_count BETWEEN 0 AND 1024),
 observed_at_ms BIGINT NOT NULL CHECK(observed_at_ms>0),
 last_output_at_ms BIGINT NOT NULL DEFAULT 0 CHECK(last_output_at_ms>=0),
 last_output_sequence BIGINT NOT NULL DEFAULT 0 CHECK(last_output_sequence>=0),
 UNIQUE(node_id,attempt_id,session_id,terminal_id)
);
CREATE INDEX terminal_node_sessions ON terminal_sessions(node_id,terminal_id);
CREATE TABLE terminal_control_acl (
 terminal_id UUID NOT NULL REFERENCES terminal_sessions(terminal_id),
 principal_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
 revision BIGINT NOT NULL CHECK(revision>0),
 can_control BOOLEAN NOT NULL,
 expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>0),
 PRIMARY KEY(terminal_id,principal_id)
);
CREATE TABLE terminal_operations (
 terminal_id UUID NOT NULL REFERENCES terminal_sessions(terminal_id),
 operation_id UUID NOT NULL,
 principal_id UUID NOT NULL,
 request_digest TEXT NOT NULL CHECK(request_digest~'^[0-9a-f]{64}$'),
 kind TEXT NOT NULL CHECK(kind IN('watch','watch_renew','unwatch','control','renew','release','capture','resolve','resize','acl','open_epoch','input','register')),
 -- Metadata-only typed replies; never an arbitrary request/input/snapshot body.
 response JSONB NOT NULL CHECK(octet_length(response::text)<=65536),
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>0),
 PRIMARY KEY(terminal_id,operation_id)
);
CREATE TABLE terminal_input_receipts (
 terminal_id UUID NOT NULL REFERENCES terminal_sessions(terminal_id),
 operation_id UUID NOT NULL,
 principal_id UUID NOT NULL,
 request_digest TEXT NOT NULL CHECK(request_digest~'^[0-9a-f]{64}$'),
 command_digest TEXT NOT NULL CHECK(command_digest~'^[0-9a-f]{64}$'),
 disposition TEXT NOT NULL CHECK(disposition IN('queued','received','injecting','applied','delivery_unknown','expired','rejected')),
 deadline_ms BIGINT NOT NULL CHECK(deadline_ms>0),
 dispatched BOOLEAN NOT NULL,
 byte_count INTEGER NOT NULL CHECK(byte_count BETWEEN 1 AND 4096),
 admission_revision BIGINT NOT NULL CHECK(admission_revision>0),
 epoch_id UUID,
 updated_at_ms BIGINT NOT NULL CHECK(updated_at_ms>0),
 PRIMARY KEY(terminal_id,operation_id)
);
CREATE TABLE terminal_subscriptions (
 terminal_id UUID NOT NULL REFERENCES terminal_sessions(terminal_id),
 subscription_id UUID NOT NULL,
 principal_id UUID NOT NULL,
 node_id UUID NOT NULL REFERENCES nodes(id),
 expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>0),
 PRIMARY KEY(terminal_id,subscription_id)
);
CREATE INDEX terminal_active_subscriptions ON terminal_subscriptions(node_id,expires_at_ms);
CREATE TABLE terminal_outbox (
 outbox_id BIGSERIAL PRIMARY KEY,
 terminal_id UUID NOT NULL REFERENCES terminal_sessions(terminal_id),
 operation_id UUID NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN('fence','capture','input','epoch_input','resize','watch')),
 controller_generation BIGINT NOT NULL CHECK(controller_generation>=0),
 ledger_revision BIGINT NOT NULL CHECK(ledger_revision>0),
 ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 63),
 expires_at_ms BIGINT,
 -- input/epoch_input rows are one-shot claims, NEVER replayable outbox work.
 claimed BOOLEAN NOT NULL,
 acknowledged BOOLEAN NOT NULL DEFAULT FALSE,
 protection_version INTEGER NOT NULL CHECK(protection_version=1),
 key_id TEXT NOT NULL CHECK(octet_length(key_id) BETWEEN 1 AND 64),
 nonce BYTEA NOT NULL CHECK(octet_length(nonce)=12),
 ciphertext BYTEA NOT NULL CHECK(octet_length(ciphertext) BETWEEN 16 AND 65552),
 UNIQUE(terminal_id,operation_id,kind)
);
CREATE INDEX terminal_pending_outbox ON terminal_outbox(terminal_id,outbox_id) WHERE NOT acknowledged;
CREATE FUNCTION terminal_binding_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='UPDATE' AND
   (NEW.terminal_id,NEW.attempt_id,NEW.node_id,NEW.boot_id,NEW.session_id,
    NEW.assignment_generation,NEW.terminal_epoch,NEW.identity,
    NEW.execution_spec_digest,NEW.launch_manifest_digest,NEW.input_mode)
   IS DISTINCT FROM
   (OLD.terminal_id,OLD.attempt_id,OLD.node_id,OLD.boot_id,OLD.session_id,
    OLD.assignment_generation,OLD.terminal_epoch,OLD.identity,
    OLD.execution_spec_digest,OLD.launch_manifest_digest,OLD.input_mode)
 THEN RAISE EXCEPTION 'immutable terminal binding' USING ERRCODE='23514'; END IF;
 IF TG_OP='UPDATE' AND (NEW.authority_revision<OLD.authority_revision OR NEW.ledger_revision<OLD.ledger_revision OR NEW.controller_generation<OLD.controller_generation)
 THEN RAISE EXCEPTION 'terminal fence regressed' USING ERRCODE='23514'; END IF;
 IF NOT EXISTS(SELECT 1 FROM execution_attempts e WHERE e.attempt_id=NEW.attempt_id AND e.node_id=NEW.node_id AND e.session_id=NEW.session_id AND e.kind='profiled_v1')
 THEN RAISE EXCEPTION 'terminal lacks profiled execution identity' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER terminal_binding_immutable BEFORE INSERT OR UPDATE ON terminal_sessions
 FOR EACH ROW EXECUTE FUNCTION terminal_binding_guard();
CREATE FUNCTION terminal_operation_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW IS DISTINCT FROM OLD THEN RAISE EXCEPTION 'immutable terminal operation receipt' USING ERRCODE='23514'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER terminal_operation_immutable BEFORE UPDATE ON terminal_operations
 FOR EACH ROW EXECUTE FUNCTION terminal_operation_guard();
UPDATE meta SET value='12' WHERE key='state_format_version';
