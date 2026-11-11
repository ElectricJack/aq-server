CREATE TABLE worker_settings (
    kind TEXT NOT NULL CHECK (kind IN ('machine','node')),
    id TEXT NOT NULL,
    label TEXT CHECK (length(label) BETWEEN 1 AND 64),
    max_agents INTEGER CHECK (max_agents >= 0),
    removed BOOLEAN NOT NULL DEFAULT FALSE,
    revision BIGINT NOT NULL CHECK (revision > 0),
    PRIMARY KEY (kind,id)
);
CREATE TABLE worker_setting_operations (
    operation_id UUID PRIMARY KEY,
    actor_id UUID NOT NULL REFERENCES users(id),
    request JSONB NOT NULL,
    receipt JSONB NOT NULL
);
CREATE VIEW worker_machine_members AS
SELECT n.id AS node_id, n.revoked_at,
       COALESCE(n.machine_key,r.report->'machine'->>'key','node:' || n.id::text) AS machine_id
FROM nodes n LEFT JOIN node_machine_reports r ON r.node_id=n.id;
UPDATE meta SET value='110' WHERE key='state_format_version';
