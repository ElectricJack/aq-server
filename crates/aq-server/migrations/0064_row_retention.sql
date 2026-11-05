-- DEP6: conservative ages start when the server first observes completion.
CREATE TABLE retention_tasks (
 workspace_id UUID NOT NULL, task_id UUID NOT NULL,
 checked_at_ms BIGINT NOT NULL DEFAULT 0,
 settled_since_ms BIGINT,
 PRIMARY KEY(workspace_id,task_id)
);
CREATE TABLE retention_closed_attempts (
 attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id),
 closed_at_ms BIGINT NOT NULL
);
-- Keep sequence watermarks and retired-attempt fences after packet receipts expire.
CREATE TABLE observation_packet_retention (
 node_id UUID NOT NULL, message_id UUID NOT NULL,
 first_sequence BIGINT NOT NULL, last_sequence BIGINT NOT NULL,
 attempt_ids UUID[] NOT NULL, recorded_at_ms BIGINT NOT NULL,
 charged_bytes BIGINT NOT NULL DEFAULT 0 CHECK(charged_bytes>=0),
 PRIMARY KEY(node_id,message_id),
 FOREIGN KEY(node_id,message_id) REFERENCES observation_packets(node_id,message_id)
);
-- Old packets have no stored charge. Preserve their conservative quota charge;
-- all new packets carry the exact charge and return it when collected.
INSERT INTO observation_packet_retention
 (node_id,message_id,first_sequence,last_sequence,attempt_ids,recorded_at_ms)
SELECT p.node_id,p.message_id,p.first_sequence,p.last_sequence,
 ARRAY(SELECT DISTINCT e.attempt_id FROM observation_events e WHERE e.node_id=p.node_id
 AND e.sequence BETWEEN p.first_sequence AND p.last_sequence),
 COALESCE((SELECT max(e.recorded_at_ms) FROM observation_events e WHERE e.node_id=p.node_id
 AND e.sequence BETWEEN p.first_sequence AND p.last_sequence),(extract(epoch FROM clock_timestamp())*1000)::bigint)
FROM (SELECT node_id,message_id,
 COALESCE(lag((receipt->>'through_sequence')::bigint) OVER(PARTITION BY node_id ORDER BY (receipt->>'through_sequence')::bigint),0)+1 AS first_sequence,
 (receipt->>'through_sequence')::bigint AS last_sequence
 FROM observation_packets) p WHERE p.last_sequence IS NOT NULL;
ALTER TABLE mailbox_messages ADD COLUMN retention_ready_at_ms BIGINT;
ALTER TABLE agent_graph_outbox ADD COLUMN retention_completed_at_ms BIGINT;
ALTER TABLE playbook_outbox ADD COLUMN retention_completed_at_ms BIGINT;
ALTER TABLE review_owner_outbox ADD COLUMN retention_completed_at_ms BIGINT;
ALTER TABLE terminal_outbox ADD COLUMN retention_completed_at_ms BIGINT;
-- Only the three history tables being collected gain a DELETE path. Updates,
-- sources, revocations, transport receipts and accounting evidence stay immutable.
CREATE FUNCTION observation_retention_delete_guard() RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' AND current_setting('aq.row_retention',true)='on' THEN RETURN OLD; END IF;
 RAISE EXCEPTION 'observation history is immutable';
END $$;
DROP TRIGGER observation_event_immutable ON observation_events;
CREATE TRIGGER observation_event_immutable BEFORE UPDATE OR DELETE ON observation_events
 FOR EACH ROW EXECUTE FUNCTION observation_retention_delete_guard();
DROP TRIGGER observation_fact_immutable ON observation_facts;
CREATE TRIGGER observation_fact_immutable BEFORE UPDATE OR DELETE ON observation_facts
 FOR EACH ROW EXECUTE FUNCTION observation_retention_delete_guard();
DROP TRIGGER observation_packet_immutable ON observation_packets;
CREATE TRIGGER observation_packet_immutable BEFORE UPDATE OR DELETE ON observation_packets
 FOR EACH ROW EXECUTE FUNCTION observation_retention_delete_guard();
CREATE INDEX retention_tasks_oldest_check ON retention_tasks(checked_at_ms);
CREATE INDEX retention_observation_age ON observation_events(recorded_at_ms);
CREATE INDEX retention_mail_age ON mailbox_messages(created_at_ms);
UPDATE meta SET value='64' WHERE key='state_format_version';
