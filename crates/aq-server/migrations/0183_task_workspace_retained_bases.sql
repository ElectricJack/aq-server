CREATE TABLE task_workspace_retained_bases (
    repository_id UUID NOT NULL,
    binding_generation BIGINT NOT NULL CHECK (binding_generation > 0),
    commit_oid TEXT NOT NULL CHECK (commit_oid ~ '^[0-9a-f]{40}$'),
    source_handle UUID NOT NULL,
    reserved_bytes BIGINT NOT NULL DEFAULT 0 CHECK (reserved_bytes >= 0),
    PRIMARY KEY (repository_id, binding_generation, commit_oid)
);

UPDATE meta SET value='183' WHERE key='state_format_version';
