-- Node observation limits become a Settings value, sources are released when
-- their attempt ends, and each attempt keeps the time of its last observation
-- and last progress for the session read. Additive: new nullable or defaulted
-- columns and one new table. The fixed caps leave the schema (the server
-- checks the Settings value under the node lock), so an older server, which
-- still checks its own constants, keeps running on the migrated schema.
SET LOCAL lock_timeout = '5s';
-- NULL means never saved: the defaults of the build (today's numbers).
ALTER TABLE role_settings
    ADD COLUMN observation_limits JSONB,
    ADD COLUMN observation_limits_revision BIGINT NOT NULL DEFAULT 0;
ALTER TABLE observation_nodes
    DROP CONSTRAINT observation_nodes_retained_bytes_check,
    DROP CONSTRAINT observation_nodes_event_count_check,
    DROP CONSTRAINT observation_nodes_source_count_check,
    ADD CONSTRAINT observation_nodes_counters_check
        CHECK(retained_bytes >= 0 AND event_count >= 0 AND source_count >= 0);
-- One row per source whose attempt ended. From then on the source no longer
-- counts against the node's source limit; its batches are accepted until
-- accept_until_ms (the final drain) and refused after. Append-only, like the
-- sources and their revocations; events stay until row retention removes them.
CREATE TABLE observation_source_releases (
    node_id UUID NOT NULL,
    source_id UUID NOT NULL,
    generation BIGINT NOT NULL,
    attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
    released_at_ms BIGINT NOT NULL CHECK(released_at_ms > 0),
    accept_until_ms BIGINT NOT NULL CHECK(accept_until_ms >= released_at_ms),
    PRIMARY KEY(node_id, source_id, generation),
    FOREIGN KEY(node_id, source_id, generation)
        REFERENCES observation_sources(node_id, source_id, generation)
);
CREATE TRIGGER observation_source_release_immutable BEFORE UPDATE OR DELETE ON observation_source_releases
    FOR EACH ROW EXECUTE FUNCTION refuse_observation_history_mutation();
-- 0 means none yet. Written by ingest for the attempts of the batch's own
-- events (last_recorded_at_ms also moves for a corrected call's owner).
ALTER TABLE observation_attempt_projections
    ADD COLUMN last_observed_at_ms BIGINT NOT NULL DEFAULT 0 CHECK(last_observed_at_ms >= 0),
    ADD COLUMN last_progress_at_ms BIGINT NOT NULL DEFAULT 0 CHECK(last_progress_at_ms >= 0);
UPDATE observation_attempt_projections p SET
    last_observed_at_ms = COALESCE((SELECT max(e.recorded_at_ms) FROM observation_events e
        WHERE e.attempt_id = p.attempt_id), 0),
    last_progress_at_ms = COALESCE((SELECT max(e.recorded_at_ms) FROM observation_events e
        WHERE e.attempt_id = p.attempt_id
        AND (e.record->'body'->'kind'->>'kind' IN ('turn_boundary','tool_started','tool_result','question')
            OR (e.record->'body'->'kind'->>'kind' = 'text_reference'
                AND e.record->'body'->'kind'->>'role' = 'assistant'))), 0);
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='150' WHERE key='state_format_version';
