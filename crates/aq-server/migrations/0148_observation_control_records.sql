-- Renumber to 0152 at merge (file name and the state_format_version below).
--
-- 1. A node restarts without limit: the transport owner's boot counter and the
--    receipt revision are no longer capped at 64. Storage is bounded by rolling
--    retention instead: each new receipt deletes the node's receipts older
--    than the newest 64 (a retention count, nothing is refused when it is
--    reached); the guard below never lets the current one go.
ALTER TABLE observation_transport_receipts
 DROP CONSTRAINT observation_transport_receipts_owner_revision_check;
ALTER TABLE observation_transport_receipts
 ADD CONSTRAINT observation_transport_receipts_owner_revision_check CHECK(owner_revision>=1);
CREATE OR REPLACE FUNCTION observation_transport_owner_guard() RETURNS TRIGGER LANGUAGE plpgsql AS $$
DECLARE revision BIGINT; transitions BIGINT;
BEGIN
 IF TG_OP='DELETE' THEN RAISE EXCEPTION 'observation owner retirement requires explicit recovery'; END IF;
 revision:=(NEW.state->>'revision')::bigint;
 transitions:=(NEW.state->>'boot_transitions')::bigint;
 IF NEW.state->'marker'->>'owner_id' IS DISTINCT FROM NEW.owner_id::text
 OR NEW.state->'marker'->>'node_id' IS DISTINCT FROM NEW.node_id::text
 OR revision IS NULL OR transitions IS NULL OR transitions<0
 THEN RAISE EXCEPTION 'observation owner identity'; END IF;
 IF TG_OP='INSERT' THEN
   IF NEW.state->>'phase' IS DISTINCT FROM 'prepared' OR revision<>1 OR transitions<>0
   OR NEW.state->>'current_boot_id' IS DISTINCT FROM NEW.state->'marker'->>'initial_boot_id'
   THEN RAISE EXCEPTION 'observation owner must start prepared'; END IF;
 ELSE
   IF NEW.node_id<>OLD.node_id OR NEW.owner_id<>OLD.owner_id
   OR NEW.state->'marker' IS DISTINCT FROM OLD.state->'marker'
   OR revision<>(OLD.state->>'revision')::bigint+1
   OR NEW.state->>'phase' IS DISTINCT FROM 'active'
   THEN RAISE EXCEPTION 'observation owner is monotonic'; END IF;
   IF OLD.state->>'phase'='prepared' THEN
     IF revision<>2 OR transitions<>0 OR NEW.state->>'current_boot_id' IS DISTINCT FROM OLD.state->>'current_boot_id'
     THEN RAISE EXCEPTION 'invalid store activation'; END IF;
   ELSE
     IF transitions<>(OLD.state->>'boot_transitions')::bigint+1
     OR NEW.state->>'current_boot_id' IS NOT DISTINCT FROM OLD.state->>'current_boot_id'
     THEN RAISE EXCEPTION 'invalid observation boot transition'; END IF;
   END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE FUNCTION observation_transport_receipt_retention_guard() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' AND OLD.owner_revision<(SELECT (state->>'revision')::bigint
   FROM observation_transport_owners WHERE node_id=OLD.node_id) THEN RETURN OLD; END IF;
 RAISE EXCEPTION 'observation history is immutable';
END $$;
DROP TRIGGER observation_transport_receipt_immutable ON observation_transport_receipts;
CREATE TRIGGER observation_transport_receipt_immutable BEFORE UPDATE OR DELETE ON observation_transport_receipts
 FOR EACH ROW EXECUTE FUNCTION observation_transport_receipt_retention_guard();
CREATE FUNCTION observation_transport_receipt_roll() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 DELETE FROM observation_transport_receipts
  WHERE node_id=NEW.node_id AND owner_revision<=NEW.owner_revision-64;
 RETURN NULL;
END $$;
CREATE TRIGGER observation_transport_receipt_rolls AFTER INSERT ON observation_transport_receipts
 FOR EACH ROW EXECUTE FUNCTION observation_transport_receipt_roll();

-- 2. A refused source no longer refuses its whole batch. The packet row keeps
--    the refusals its reply listed (replayed with the receipt); a refusal of an
--    enrolled source is also recorded against the source, once, and stands.
ALTER TABLE observation_packets
 ADD COLUMN refused JSONB CHECK(refused IS NULL OR octet_length(refused::text)<=8192);
CREATE TABLE observation_source_refusals (
 node_id UUID NOT NULL,
 source_id UUID NOT NULL,
 generation BIGINT NOT NULL,
 reason TEXT NOT NULL CHECK(reason IN ('binding','provenance','parser','control_shape')),
 message_id UUID NOT NULL,
 sequence BIGINT NOT NULL CHECK(sequence>0),
 refused_at_ms BIGINT NOT NULL CHECK(refused_at_ms>0),
 PRIMARY KEY(node_id,source_id,generation),
 FOREIGN KEY(node_id,source_id,generation) REFERENCES observation_sources(node_id,source_id,generation)
);
CREATE TRIGGER observation_source_refusal_immutable BEFORE UPDATE OR DELETE ON observation_source_refusals
 FOR EACH ROW EXECUTE FUNCTION refuse_observation_history_mutation();
UPDATE meta SET value='148' WHERE key='state_format_version';
