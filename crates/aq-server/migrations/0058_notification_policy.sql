-- F4 automatic notification policy. One notification per episode (kind +
-- project + subject); flood windows collapse bursts into one counted summary.
-- Message content and delivery remain owned by the AQ24 notification tables.
CREATE TABLE notification_episodes (
 episode_key TEXT PRIMARY KEY CHECK(octet_length(episode_key) BETWEEN 1 AND 512),
 kind TEXT NOT NULL CHECK(kind IN ('task.failed_or_blocked','decision.needed','question.asked','credential.invalid','milestone.gate_ready','worker.offline')),
 subject TEXT NOT NULL CHECK(octet_length(subject) BETWEEN 1 AND 256),
 project_id UUID NOT NULL,
 workspace_id UUID NOT NULL,
 generation BIGINT NOT NULL CHECK(generation>0),
 open BOOLEAN NOT NULL,
 opened_at_ms BIGINT NOT NULL CHECK(opened_at_ms>0),
 resolved_at_ms BIGINT CHECK(resolved_at_ms>0),
 outcome TEXT NOT NULL CHECK(outcome IN ('notified','collapsed')),
 request JSONB CHECK(request IS NULL OR octet_length(request::text)<=16384),
 CHECK((outcome='notified')=(request IS NOT NULL))
);
CREATE TABLE notification_policy_events (
 source_event_id UUID PRIMARY KEY,
 request_digest TEXT NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
 response JSONB NOT NULL CHECK(octet_length(response::text)<=16384),
 request JSONB CHECK(request IS NULL OR octet_length(request::text)<=16384)
);
CREATE TABLE notification_flood_windows (
 project_id UUID NOT NULL,
 workspace_id UUID NOT NULL,
 window_start_ms BIGINT NOT NULL CHECK(window_start_ms>=0),
 notified INTEGER NOT NULL DEFAULT 0 CHECK(notified>=0),
 collapsed INTEGER NOT NULL DEFAULT 0 CHECK(collapsed>=0),
 kinds TEXT[] NOT NULL DEFAULT '{}',
 summary JSONB CHECK(summary IS NULL OR octet_length(summary::text)<=16384),
 submitted BOOLEAN NOT NULL DEFAULT FALSE,
 PRIMARY KEY(project_id,workspace_id,window_start_ms)
);
CREATE INDEX notification_flood_windows_pending ON notification_flood_windows(window_start_ms) WHERE collapsed>0 AND NOT submitted;
UPDATE meta SET value='58' WHERE key='state_format_version';
