-- Following main (spec 0016 section 4, feature delivery_playbook). Each head
-- central aq-git synced from a bound project's default branch and retained
-- (repo.retain_ref), one row per retention handle. The latest is the row with
-- the greatest synced_at_ms. An offer starts from it, and policy validation
-- accepts a handle recorded here for exactly its head. Additive: an older
-- server never reads this table.
CREATE TABLE profiled_main_syncs (
    project_id UUID NOT NULL,
    repository_id UUID NOT NULL,
    binding_generation BIGINT NOT NULL CHECK (binding_generation > 0),
    reference TEXT NOT NULL
        CHECK (reference LIKE 'refs/heads/%' AND length(reference) <= 1024),
    retention_handle UUID NOT NULL,
    head TEXT NOT NULL CHECK (head ~ '^[0-9a-f]{40}$'),
    synced_at_ms BIGINT NOT NULL CHECK (synced_at_ms > 0),
    recorded_at_ms BIGINT NOT NULL,
    PRIMARY KEY (project_id, repository_id, binding_generation, reference, retention_handle)
);
CREATE INDEX profiled_main_syncs_latest ON profiled_main_syncs
    (project_id, repository_id, binding_generation, reference, synced_at_ms DESC);

UPDATE meta SET value='143' WHERE key='state_format_version';
