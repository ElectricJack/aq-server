-- Spec 0016 §4 (feature `delivery_playbook`): every handle aq-git's
-- `repo.retain_ref` returned for a bound project's named ref, with the head it
-- retains. The newest `synced_at_ms` per project, repository, generation and
-- reference is the latest sync. Older rows stay so that a source offered
-- before the ref moved can still be exported from its own handle.
CREATE TABLE main_sync_heads (
    project_id UUID NOT NULL,
    repository_id UUID NOT NULL,
    binding_generation BIGINT NOT NULL CHECK (binding_generation > 0),
    reference TEXT NOT NULL CHECK (reference LIKE 'refs/%' AND length(reference) <= 4096),
    retention_handle UUID NOT NULL,
    head_hex TEXT NOT NULL CHECK (head_hex ~ '^[0-9a-f]{40}$'),
    synced_at_ms BIGINT NOT NULL CHECK (synced_at_ms > 0),
    operation_id UUID NOT NULL,
    PRIMARY KEY (project_id, repository_id, binding_generation, reference, retention_handle)
);
CREATE INDEX main_sync_heads_latest
    ON main_sync_heads (project_id, repository_id, binding_generation, reference, synced_at_ms DESC);

UPDATE meta SET value='141' WHERE key='state_format_version';
