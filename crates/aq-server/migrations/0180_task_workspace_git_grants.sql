-- An approved workspace is frozen once for each native attempt. Later reads
-- must still verify the current source approval and project binding.
CREATE TABLE task_workspace_git_grants (
    attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
    home_project_id UUID NOT NULL,
    task_id UUID NOT NULL,
    target_project_id UUID NOT NULL,
    project_revision BIGINT NOT NULL CHECK (project_revision > 0),
    repository_id UUID NOT NULL,
    binding_generation BIGINT NOT NULL CHECK (binding_generation > 0),
    commit_oid TEXT NOT NULL CHECK (commit_oid ~ '^[0-9a-f]{40}$'),
    access TEXT NOT NULL CHECK (access IN ('read', 'write')),
    task_ref TEXT NOT NULL CHECK (task_ref ~ '^refs/heads/aq/[0-9a-f-]{36}$'),
    checkout_head TEXT NOT NULL CHECK (checkout_head ~ '^[0-9a-f]{40}$'),
    lease TEXT CHECK (lease IS NULL OR (access = 'write' AND lease = checkout_head)),
    CHECK (checkout_head = commit_oid OR (access = 'write' AND lease IS NOT NULL)),
    PRIMARY KEY (attempt_id, target_project_id),
    UNIQUE (attempt_id, repository_id)
);
CREATE TRIGGER task_workspace_git_grants_immutable
    BEFORE UPDATE ON task_workspace_git_grants
    FOR EACH ROW EXECUTE FUNCTION review_immutable_record();

UPDATE meta SET value='180' WHERE key='state_format_version';
