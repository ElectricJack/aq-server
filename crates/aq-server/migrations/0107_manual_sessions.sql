-- Owner-started interactive sessions are not queue tasks or finite task grants.
CREATE TABLE manual_worker_reports (
 node_id UUID PRIMARY KEY REFERENCES nodes(id),
 max_slots INTEGER NOT NULL CHECK(max_slots BETWEEN 1 AND 8),
 repositories JSONB NOT NULL CHECK(jsonb_typeof(repositories)='array'),
 observed_at_ms BIGINT NOT NULL
);
CREATE TABLE manual_project_slots (
 node_id UUID NOT NULL REFERENCES nodes(id),
 project_id UUID NOT NULL REFERENCES project_execution_bindings(project_id),
 slots INTEGER NOT NULL CHECK(slots BETWEEN 0 AND 8),
 revision BIGINT NOT NULL CHECK(revision>0),
 PRIMARY KEY(node_id,project_id)
);
CREATE TABLE manual_agent_sessions (
 id UUID PRIMARY KEY,
 actor_id UUID NOT NULL REFERENCES users(id),
 node_id UUID NOT NULL REFERENCES nodes(id),
 project_id UUID NOT NULL REFERENCES project_execution_bindings(project_id),
 spec JSONB NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('pending','preparing','running','stopping','stopped','failed')),
 stage TEXT NOT NULL CHECK(length(stage)<=128),
 desired_running BOOLEAN NOT NULL DEFAULT TRUE,
 created_at_ms BIGINT NOT NULL,
 updated_at_ms BIGINT NOT NULL,
 worker_seen_at_ms BIGINT,
 stopped_at_ms BIGINT
);
CREATE INDEX manual_agent_sessions_live ON manual_agent_sessions(node_id,project_id) WHERE stopped_at_ms IS NULL;
CREATE TABLE manual_agent_operations (
 operation_id UUID PRIMARY KEY,
 actor_id UUID NOT NULL REFERENCES users(id),
 request JSONB NOT NULL,
 receipt JSONB NOT NULL
);
UPDATE meta SET value='107' WHERE key='state_format_version';
