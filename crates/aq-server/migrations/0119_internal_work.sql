-- Internal work (spec 0012 §4): model requests made by AQ itself, kept as
-- server records outside the task graph and outside project capacity.
CREATE TABLE internal_tasks (
    id UUID PRIMARY KEY,
    purpose TEXT NOT NULL,
    -- The first version has one kind; the column exists so a second kind
    -- (a repository to read) does not change the first.
    workspace_kind TEXT NOT NULL DEFAULT 'none' CHECK(workspace_kind IN ('none')),
    request JSONB NOT NULL,
    -- Resolved profile entries in fallback order; candidate_index is the one
    -- that holds or last held the task.
    candidates JSONB NOT NULL CHECK(jsonb_typeof(candidates)='array'),
    candidate_index INT NOT NULL DEFAULT 0 CHECK(candidate_index>=0),
    status TEXT NOT NULL CHECK(status IN ('queued','running','answered','failed')),
    blocking BOOLEAN NOT NULL,
    runner TEXT,
    claimed_at_ms BIGINT,
    deadline_at_ms BIGINT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    attempts JSONB NOT NULL DEFAULT '[]' CHECK(jsonb_typeof(attempts)='array'),
    answer JSONB,
    usage JSONB,
    reason TEXT,
    detail TEXT,
    affected_kind TEXT NOT NULL,
    affected_id TEXT NOT NULL,
    project_id UUID,
    escalate BOOLEAN NOT NULL,
    escalation TEXT NOT NULL DEFAULT 'none' CHECK(escalation IN ('none','supervisor','owner','resolved')),
    supervisor_attempts INT NOT NULL DEFAULT 0,
    diagnosis TEXT,
    retry_of UUID
);
CREATE INDEX internal_tasks_queue ON internal_tasks(status, blocking DESC, created_at_ms);
CREATE INDEX internal_tasks_affected ON internal_tasks(affected_kind, affected_id);
UPDATE meta SET value='119' WHERE key='state_format_version';
