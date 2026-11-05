-- Central-only aq-git whitelist. Inputs and exact wire frames survive restarts.
CREATE TABLE delivery_intents (
 operation_id UUID PRIMARY KEY,
 repository_id UUID NOT NULL,
 binding_generation BIGINT NOT NULL CHECK (binding_generation > 0),
 operation TEXT NOT NULL CHECK (operation IN ('candidate.create','candidate.show','publish','operation.show')),
 inputs JSONB NOT NULL,
 input_digest TEXT NOT NULL CHECK (input_digest ~ '^[0-9a-f]{64}$'),
 payload_digest TEXT NOT NULL CHECK (payload_digest ~ '^[0-9a-f]{64}$'),
 request BYTEA NOT NULL CHECK (octet_length(request) BETWEEN 1 AND 1048576),
 created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE delivery_receipts (
 operation_id UUID PRIMARY KEY REFERENCES delivery_intents(operation_id),
 response BYTEA NOT NULL CHECK (octet_length(response) BETWEEN 1 AND 8388608),
 received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- A committed marker without a receipt is unknown, even if the daemon died
-- before sending bytes. No retry of publish is allowed to send again.
CREATE TABLE delivery_publish_dispatches (
 operation_id UUID PRIMARY KEY REFERENCES delivery_intents(operation_id),
 started_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Deliberately independent of binding generation: rebinding cannot evade a hold.
CREATE TABLE delivery_destinations (
 repository_id UUID NOT NULL,
 target TEXT NOT NULL CHECK (target LIKE 'refs/heads/%'),
 operation_id UUID NOT NULL UNIQUE REFERENCES delivery_intents(operation_id),
 PRIMARY KEY (repository_id,target)
);
CREATE FUNCTION delivery_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 RAISE EXCEPTION 'delivery evidence is immutable';
END $$;
CREATE TRIGGER delivery_intent_immutable BEFORE UPDATE OR DELETE ON delivery_intents
 FOR EACH ROW EXECUTE FUNCTION delivery_immutable();
CREATE TRIGGER delivery_receipt_immutable BEFORE UPDATE OR DELETE ON delivery_receipts
 FOR EACH ROW EXECUTE FUNCTION delivery_immutable();
CREATE TRIGGER delivery_dispatch_immutable BEFORE UPDATE OR DELETE ON delivery_publish_dispatches
 FOR EACH ROW EXECUTE FUNCTION delivery_immutable();
UPDATE meta SET value='45' WHERE key='state_format_version';
