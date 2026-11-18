-- Git grants (spec 0016 section 5.2, feature delivery_playbook). One row per
-- attempt whose offer carried a git grant: what the server tells central
-- aq-git on each of the attempt's fetches and pushes through /v1/git. The
-- token itself is never stored; it is derived from the delegated permit key.
-- Written in the offer's publication transaction. Additive: an older server
-- never reads this table.
CREATE TABLE delivery_git_grants (
    attempt_id UUID PRIMARY KEY,
    node_id UUID NOT NULL,
    repository_id UUID NOT NULL,
    base TEXT NOT NULL CHECK (base ~ '^[0-9a-f]{40}$'),
    task_ref TEXT NOT NULL
        CHECK (task_ref LIKE 'refs/heads/aq/%' AND length(task_ref) <= 1024),
    lease TEXT CHECK (lease ~ '^[0-9a-f]{40}$'),
    main_ref TEXT NOT NULL
        CHECK (main_ref LIKE 'refs/heads/%' AND length(main_ref) <= 1024),
    operation_id UUID NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

UPDATE meta SET value='146' WHERE key='state_format_version';
