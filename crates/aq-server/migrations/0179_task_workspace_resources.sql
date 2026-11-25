-- The home Graph task keeps its identity; participating repositories are AQ resources.
CREATE TABLE task_workspace_sources (
    home_project_id uuid NOT NULL REFERENCES project_execution_bindings(project_id),
    task_id uuid NOT NULL CHECK(task_id <> '00000000-0000-0000-0000-000000000000'::uuid),
    target_project_id uuid NOT NULL REFERENCES project_execution_bindings(project_id),
    project_revision bigint NOT NULL CHECK(project_revision > 0),
    repository_id uuid NOT NULL,
    binding_generation bigint NOT NULL CHECK(binding_generation > 0),
    commit_oid text NOT NULL CHECK(commit_oid ~ '^[0-9a-f]{40}$' AND commit_oid <> repeat('0',40)),
    access text NOT NULL CHECK(access IN ('read','write')),
    state text NOT NULL CHECK(state IN ('pending','approved','denied')),
    requested_attempt_id uuid REFERENCES execution_attempts(attempt_id),
    approved_by uuid REFERENCES users(id),
    PRIMARY KEY (home_project_id,task_id,target_project_id),
    UNIQUE(home_project_id,task_id,repository_id),
    CHECK(home_project_id <> target_project_id)
);
CREATE INDEX task_workspace_inbox ON task_workspace_sources(target_project_id,home_project_id,task_id)
    WHERE state='pending';
CREATE TABLE task_workspace_services (
    home_project_id uuid NOT NULL REFERENCES project_execution_bindings(project_id),
    task_id uuid NOT NULL CHECK(task_id <> '00000000-0000-0000-0000-000000000000'::uuid),
    service text NOT NULL CHECK(service='postgres'),
    requested_attempt_id uuid REFERENCES execution_attempts(attempt_id),
    PRIMARY KEY(home_project_id,task_id,service)
);
-- Stage pins before Graph can expose a ready task. Only the confirmed Graph
-- receipt can activate the pins. A rejected create discards the stage.
CREATE TABLE task_workspace_creations (
    task_id uuid PRIMARY KEY REFERENCES human_graph_operations(operation_id) ON DELETE CASCADE,
    home_project_id uuid NOT NULL REFERENCES project_execution_bindings(project_id),
    requirements jsonb NOT NULL CHECK(jsonb_typeof(requirements)='array' AND jsonb_array_length(requirements) BETWEEN 1 AND 17)
);
UPDATE meta SET value='179' WHERE key='state_format_version';
