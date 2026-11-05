-- H5 server side: CON-03 suspension directives, parking reports, continuation
-- attachments and bootstrap acknowledgments. One row per H4 continuation; the
-- packets the worker sends are deduplicated by message_id + body_digest.
CREATE TABLE hibernation_suspensions (
 continuation_id UUID PRIMARY KEY,
 wait_id UUID NOT NULL,
 predecessor_attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
 predecessor_node_id UUID NOT NULL,
 intent JSONB NOT NULL,
 directive_packet JSONB NOT NULL,
 phase TEXT NOT NULL CHECK (phase IN ('directed','parked_held','parked_retained','continued','acknowledged')),
 checkpoint_message_id UUID,
 checkpoint JSONB,
 parking_message_id UUID,
 parking JSONB,
 disk_release_directive JSONB,
 disk_released BOOLEAN NOT NULL DEFAULT false,
 successor_attempt_id UUID REFERENCES execution_attempts(attempt_id),
 attachment JSONB,
 attachment_digest TEXT,
 continuation_packet JSONB,
 ack_message_id UUID,
 ack JSONB,
 created_at_ms BIGINT NOT NULL,
 updated_at_ms BIGINT NOT NULL,
 CHECK ((successor_attempt_id IS NULL) = (attachment IS NULL)),
 CHECK (phase <> 'acknowledged' OR ack IS NOT NULL)
);
-- At most one unacknowledged suspension per predecessor attempt.
CREATE UNIQUE INDEX hibernation_one_open_per_attempt ON hibernation_suspensions (predecessor_attempt_id) WHERE phase <> 'acknowledged';
CREATE UNIQUE INDEX hibernation_successor ON hibernation_suspensions (successor_attempt_id) WHERE successor_attempt_id IS NOT NULL;

CREATE TABLE hibernation_receipts (
 message_id UUID PRIMARY KEY,
 continuation_id UUID NOT NULL REFERENCES hibernation_suspensions(continuation_id),
 body_digest TEXT NOT NULL,
 packet JSONB NOT NULL,
 receipt JSONB NOT NULL,
 recorded_at_ms BIGINT NOT NULL
);
CREATE FUNCTION hibernation_immutable_receipt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'immutable hibernation receipt'; END $$;
CREATE TRIGGER hibernation_receipts_immutable BEFORE UPDATE OR DELETE ON hibernation_receipts
 FOR EACH ROW EXECUTE FUNCTION hibernation_immutable_receipt();
UPDATE meta SET value='87' WHERE key='state_format_version';
