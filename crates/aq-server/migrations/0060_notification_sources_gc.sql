-- Independent notification fanout: the playbook engine owns `delivered`.
ALTER TABLE agent_graph_outbox ADD COLUMN notification_observed BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE agent_graph_outbox ADD COLUMN notification_events JSONB;
CREATE INDEX agent_graph_notification_pending ON agent_graph_outbox(operation_id) WHERE NOT notification_observed;
-- Retain pending submissions across crashes; collect only completed receipts.
ALTER TABLE notification_policy_events ADD COLUMN created_at_ms BIGINT NOT NULL DEFAULT (extract(epoch FROM clock_timestamp())*1000)::bigint;
ALTER TABLE notification_policy_events ADD COLUMN submitted BOOLEAN NOT NULL DEFAULT FALSE;
UPDATE notification_policy_events e SET submitted=TRUE WHERE request IS NULL OR EXISTS (
 SELECT 1 FROM notification_operations o WHERE o.operation_id=(e.request->>'operation_id')::uuid
);
CREATE INDEX notification_policy_events_gc ON notification_policy_events(created_at_ms) WHERE submitted;
ALTER TABLE notification_episodes ADD COLUMN identity UUID;
UPDATE notification_episodes SET identity=md5(episode_key)::uuid;
ALTER TABLE notification_episodes ALTER COLUMN identity SET NOT NULL;
CREATE INDEX notification_episodes_gc ON notification_episodes(resolved_at_ms) WHERE NOT open;
UPDATE meta SET value='60' WHERE key='state_format_version';
