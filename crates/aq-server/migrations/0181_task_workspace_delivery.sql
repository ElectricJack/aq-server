-- Retained result facts for writable workspace grants. The outcome is
-- immutable once committed; a retry with another attempt uses a new row.
CREATE TABLE task_workspace_result_sources (
    attempt_id uuid NOT NULL,
    target_project_id uuid NOT NULL,
    result_head text CHECK (result_head ~ '^[0-9a-f]{40}([0-9a-f]{24})?$' AND result_head !~ '^0+$'),
    status text NOT NULL CHECK (status IN ('unchanged','pushed','conflict')),
    recorded_at_ms bigint NOT NULL CHECK (recorded_at_ms > 0),
    PRIMARY KEY (attempt_id,target_project_id),
    FOREIGN KEY (attempt_id,target_project_id) REFERENCES task_workspace_git_grants(attempt_id,target_project_id) ON DELETE CASCADE,
    CHECK ((status='conflict')=(result_head IS NULL))
);
CREATE TRIGGER task_workspace_result_sources_immutable
    BEFORE UPDATE ON task_workspace_result_sources
    FOR EACH ROW EXECUTE FUNCTION review_immutable_record();

-- Approval is keyed by the task and a complete frozen target identity. The
-- same independently accepted target can be reused after another repo retries.
CREATE TABLE task_workspace_target_acceptance (
    operation_id uuid PRIMARY KEY,
    home_project_id uuid NOT NULL,
    task_id uuid NOT NULL,
    target_project_id uuid NOT NULL,
    project_revision bigint NOT NULL,
    repository_id uuid NOT NULL,
    binding_generation bigint NOT NULL,
    result_head text NOT NULL CHECK (result_head ~ '^[0-9a-f]{40}([0-9a-f]{24})?$' AND result_head !~ '^0+$'),
    target_ref text,
    kind text NOT NULL CHECK (kind IN ('owner_acceptance','external_delivery','verified_merge')),
    evidence text NOT NULL CHECK (octet_length(evidence) BETWEEN 1 AND 2048),
    actor_id uuid NOT NULL REFERENCES users(id),
    recorded_at_ms bigint NOT NULL CHECK (recorded_at_ms > 0)
);
CREATE INDEX task_workspace_target_acceptance_lookup ON task_workspace_target_acceptance
    (home_project_id,task_id,target_project_id,project_revision,repository_id,binding_generation,result_head);
CREATE TRIGGER task_workspace_target_acceptance_immutable
    BEFORE UPDATE OR DELETE ON task_workspace_target_acceptance
    FOR EACH ROW EXECUTE FUNCTION review_immutable_record();

-- The server derives this digest from every checkout's identity and commit.
-- Integrated checks can be reused only while the whole set is identical.
CREATE TABLE task_workspace_integrated_checks (
    operation_id uuid PRIMARY KEY,
    sequence bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
    home_project_id uuid NOT NULL,
    task_id uuid NOT NULL,
    inputs_digest text NOT NULL CHECK (inputs_digest ~ '^[0-9a-f]{64}$'),
    verdict text NOT NULL CHECK (verdict IN ('passed','failed')),
    evidence text NOT NULL CHECK (octet_length(evidence) BETWEEN 1 AND 2048),
    actor_id uuid NOT NULL REFERENCES users(id),
    recorded_at_ms bigint NOT NULL CHECK (recorded_at_ms > 0)
);
CREATE INDEX task_workspace_integrated_checks_lookup ON task_workspace_integrated_checks
    (home_project_id,task_id,inputs_digest,sequence DESC);
CREATE TRIGGER task_workspace_integrated_checks_immutable
    BEFORE UPDATE OR DELETE ON task_workspace_integrated_checks
    FOR EACH ROW EXECUTE FUNCTION review_immutable_record();

-- One retained publication intent per exact target result. A separate repo's
-- failure can be retried without redoing an already published/opened target.
CREATE TABLE task_workspace_publications (
    operation_id uuid PRIMARY KEY,
    home_project_id uuid NOT NULL,
    task_id uuid NOT NULL,
    target_project_id uuid NOT NULL,
    project_revision bigint NOT NULL,
    repository_id uuid NOT NULL,
    binding_generation bigint NOT NULL,
    result_head text NOT NULL CHECK (result_head ~ '^[0-9a-f]{40}([0-9a-f]{24})?$' AND result_head !~ '^0+$'),
    target_ref text NOT NULL,
    expected_old text CHECK (expected_old ~ '^[0-9a-f]{40}([0-9a-f]{24})?$' AND expected_old !~ '^0+$'),
    opened_at_ms bigint CHECK (opened_at_ms > 0),
    published_at_ms bigint CHECK (published_at_ms > 0),
    pull_request bigint CHECK (pull_request BETWEEN 1 AND 1000000000),
    pull_request_url text CHECK (octet_length(pull_request_url) <= 2048),
    actor_id uuid NOT NULL REFERENCES users(id),
    created_at_ms bigint NOT NULL CHECK (created_at_ms > 0),
    UNIQUE (home_project_id,task_id,target_project_id,project_revision,repository_id,binding_generation,result_head,target_ref),
    CHECK ((pull_request IS NULL)=(opened_at_ms IS NULL) AND (pull_request IS NULL)=(pull_request_url IS NULL)),
    CHECK (opened_at_ms IS NULL OR published_at_ms IS NOT NULL)
);
CREATE INDEX task_workspace_publications_task_ref ON task_workspace_publications
    (home_project_id,task_id,target_project_id,repository_id,binding_generation,published_at_ms DESC);

UPDATE meta SET value='181' WHERE key='state_format_version';
