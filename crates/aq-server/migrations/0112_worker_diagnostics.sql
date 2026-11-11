-- Fixed owner-requested diagnostics and receiver updates; no arbitrary host commands.
CREATE TABLE worker_diagnostic_receivers (
    node_id uuid PRIMARY KEY REFERENCES nodes(id),
    seen_at_ms bigint NOT NULL,
    version integer NOT NULL CHECK (version > 0)
);
CREATE TABLE worker_diagnostic_jobs (
    id uuid PRIMARY KEY,
    node_id uuid NOT NULL REFERENCES nodes(id),
    actor_id uuid NOT NULL REFERENCES users(id),
    kind text NOT NULL CHECK (kind IN ('sandbox_startup_v1','receiver_update_v1')),
    installer_sha256 text,
    status text NOT NULL CHECK (status IN ('queued','running','completed')),
    created_at_ms bigint NOT NULL,
    updated_at_ms bigint NOT NULL,
    result jsonb,
    CHECK ((kind='receiver_update_v1') = (installer_sha256 IS NOT NULL)),
    CHECK (result IS NULL OR octet_length(result::text)<=32768)
);
CREATE UNIQUE INDEX worker_diagnostic_active ON worker_diagnostic_jobs(node_id) WHERE status IN ('queued','running');
UPDATE meta SET value='112' WHERE key='state_format_version';
