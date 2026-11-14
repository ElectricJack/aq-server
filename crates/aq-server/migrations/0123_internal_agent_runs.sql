-- Internal work in agent mode (spec 0013 §4.2): a harness with tools runs in a
-- scratch workspace until it writes its outcome. A task carrying agent work
-- has workspace kind 'agent'. Existing rows are all 'none' and stay valid.
ALTER TABLE internal_tasks DROP CONSTRAINT internal_tasks_workspace_kind_check;
ALTER TABLE internal_tasks ADD CONSTRAINT internal_tasks_workspace_kind_check
    CHECK (workspace_kind IN ('none','agent'));
-- What each candidate's agent did: its harness's events, clipped and
-- redacted by the host before they are stored.
CREATE TABLE internal_task_transcripts (
    task_id UUID NOT NULL REFERENCES internal_tasks(id) ON DELETE CASCADE,
    candidate_index INT NOT NULL CHECK (candidate_index >= 0),
    events JSONB NOT NULL CHECK (jsonb_typeof(events)='array'),
    bytes INT NOT NULL CHECK (bytes BETWEEN 0 AND 262144),
    PRIMARY KEY (task_id, candidate_index)
);
UPDATE meta SET value='123' WHERE key='state_format_version';
