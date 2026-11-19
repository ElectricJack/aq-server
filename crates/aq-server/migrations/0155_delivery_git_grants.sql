-- Git grants (spec 0016 section 5.2, feature delivery_playbook). One row per
-- attempt offered with a git grant: what the server tells central aq-git on
-- each of the attempt's fetches and pushes through /v1/git, the task ref's
-- head when the offer was made (the push lease), and whether a returned task
-- merges main first. The node reads it with GET /v1/git/grants/<attempt>. The node
-- authenticates with its own credential; the attempt's node and its hold on
-- the task come from execution_attempts. Written in the offer's publication
-- transaction. Additive: an older server never reads this table.
CREATE TABLE delivery_git_grants (
    attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
    repository_id UUID NOT NULL,
    base TEXT NOT NULL CHECK (base ~ '^[0-9a-f]{40}$'),
    task_ref TEXT NOT NULL
        CHECK (task_ref LIKE 'refs/heads/aq/%' AND length(task_ref) <= 1024),
    lease TEXT CHECK (lease ~ '^[0-9a-f]{40}$'),
    update_from_main BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

UPDATE meta SET value='155' WHERE key='state_format_version';
