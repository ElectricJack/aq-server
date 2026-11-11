CREATE TABLE worker_host_receivers (
 node_id UUID PRIMARY KEY REFERENCES nodes(id),
 seen_at_ms BIGINT NOT NULL
);
CREATE TABLE worker_host_shells (
 id UUID PRIMARY KEY,
 node_id UUID NOT NULL REFERENCES nodes(id),
 actor_id UUID NOT NULL,
 desired_running BOOLEAN NOT NULL DEFAULT TRUE,
 status TEXT NOT NULL DEFAULT 'starting' CHECK(status IN ('starting','running','ended')),
 created_at_ms BIGINT NOT NULL,
 updated_at_ms BIGINT NOT NULL,
 worker_seen_at_ms BIGINT
);
CREATE UNIQUE INDEX worker_host_shell_active_node ON worker_host_shells(node_id) WHERE desired_running;
CREATE TABLE worker_host_operations (
 id UUID PRIMARY KEY, actor_id UUID NOT NULL, node_id UUID NOT NULL REFERENCES nodes(id), shell_id UUID NOT NULL REFERENCES worker_host_shells(id)
);
UPDATE meta SET value='114' WHERE key='state_format_version';
