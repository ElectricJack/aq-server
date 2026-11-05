-- H2 follow-up: observation source generations beyond 1 and a lineage column
-- for lineage budget reads.
--
-- Sources stay immutable history. Re-enrolling an attempt after a catalog or
-- credential change, or a node move, registers the next generation and records
-- an append-only revocation of the previous one. Late usage from a revoked
-- source is recorded but never charged.
CREATE TABLE observation_source_revocations (
 node_id UUID NOT NULL,
 source_id UUID NOT NULL,
 generation BIGINT NOT NULL CHECK(generation > 0),
 attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
 superseded_by_node_id UUID NOT NULL,
 superseded_by_source_id UUID NOT NULL,
 superseded_by_generation BIGINT NOT NULL CHECK(superseded_by_generation > generation),
 revoked_at_ms BIGINT NOT NULL CHECK(revoked_at_ms > 0),
 PRIMARY KEY(node_id, source_id, generation),
 FOREIGN KEY(node_id, source_id, generation)
  REFERENCES observation_sources(node_id, source_id, generation),
 FOREIGN KEY(superseded_by_node_id, superseded_by_source_id, superseded_by_generation)
  REFERENCES observation_sources(node_id, source_id, generation)
);
CREATE TRIGGER observation_source_revocation_immutable BEFORE UPDATE OR DELETE ON observation_source_revocations
 FOR EACH ROW EXECUTE FUNCTION refuse_observation_history_mutation();

-- A canonical call's lineage is fixed by its first fact (reconcile refuses a
-- current fact from another lineage), so it is set with the owner and never
-- changes. Budget reads filter on it instead of joining every lineage event.
ALTER TABLE observation_call_heads ADD COLUMN lineage_id UUID;
UPDATE observation_call_heads SET lineage_id=(fact->>'lineage')::uuid WHERE fact IS NOT NULL;
ALTER TABLE observation_call_heads
 ADD CONSTRAINT observation_call_heads_lineage_set CHECK((fact IS NULL) = (lineage_id IS NULL));
CREATE OR REPLACE FUNCTION observation_call_owner_guard() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.owner_attempt_id IS NOT NULL AND NEW.owner_attempt_id IS DISTINCT FROM OLD.owner_attempt_id
 THEN RAISE EXCEPTION 'canonical call ownership is immutable'; END IF;
 IF OLD.lineage_id IS NOT NULL AND NEW.lineage_id IS DISTINCT FROM OLD.lineage_id
 THEN RAISE EXCEPTION 'canonical call lineage is immutable'; END IF;
 RETURN NEW;
END $$;
CREATE INDEX observation_call_heads_lineage ON observation_call_heads(lineage_id, call_key)
 WHERE lineage_id IS NOT NULL;
UPDATE meta SET value='57' WHERE key='state_format_version';
