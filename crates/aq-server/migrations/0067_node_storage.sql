-- SBX25 measured storage: the latest report per node (subject = node id) and
-- for the central host (subject = 'central'). Placement refuses above 85%;
-- the F4 storage.pressure episode opens at 80% and resolves below 75%.
CREATE TABLE node_storage_reports (
 subject TEXT PRIMARY KEY CHECK(octet_length(subject) BETWEEN 1 AND 64),
 node_id UUID REFERENCES nodes(id) ON DELETE CASCADE,
 used_bps INTEGER NOT NULL CHECK(used_bps BETWEEN 0 AND 10000),
 report JSONB NOT NULL CHECK(octet_length(report::text)<=4096),
 observed_at_ms BIGINT NOT NULL CHECK(observed_at_ms>=0),
 CHECK((subject='central')=(node_id IS NULL)),
 CHECK(node_id IS NULL OR subject=node_id::text)
);
CREATE INDEX node_storage_reports_node ON node_storage_reports(node_id) WHERE node_id IS NOT NULL;
ALTER TABLE notification_episodes DROP CONSTRAINT notification_episodes_kind_check;
ALTER TABLE notification_episodes ADD CONSTRAINT notification_episodes_kind_check CHECK(kind IN ('task.failed_or_blocked','decision.needed','question.asked','credential.invalid','milestone.gate_ready','worker.offline','storage.pressure','sandbox.preserved'));
UPDATE meta SET value='67' WHERE key='state_format_version';
