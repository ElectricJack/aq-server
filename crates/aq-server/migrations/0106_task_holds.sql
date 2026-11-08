-- R6 (lessons W83, W87): a server-side decision that keeps a ready task from
-- being offered (an exhausted retry, time or cost budget, a stuck successor) is
-- recorded on the task once, so the owner sees why it is not running. A hold
-- belongs to one attempt history: `attempts` is the task's attempt count when it
-- was recorded, so any new attempt of the task leaves it behind. `reason` is a
-- server reason code, never worker text; `comment_id` is the one system comment
-- that names it (posted once, `commented` once it is on record).
CREATE TABLE task_holds (
  task_id UUID NOT NULL,
  attempts INTEGER NOT NULL CHECK (attempts > 0),
  project_id UUID NOT NULL,
  reason TEXT NOT NULL CHECK (reason ~ '^[a-z][a-z0-9_]{0,95}$'),
  attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
  comment_id UUID NOT NULL UNIQUE,
  commented BOOLEAN NOT NULL DEFAULT FALSE,
  held_at TIMESTAMPTZ NOT NULL,
  -- The last poll that made the same decision (refreshed at most once a minute).
  seen_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (task_id, attempts)
);
CREATE INDEX task_holds_project ON task_holds(project_id, seen_at);
-- Consecutive polls that skipped the successor of one predecessor (a refused
-- handoff export or a refused selection). Cleared when a successor of the task
-- passes both; a new predecessor starts its own count.
CREATE TABLE successor_skips (
  task_id UUID NOT NULL,
  predecessor UUID NOT NULL REFERENCES execution_attempts(attempt_id),
  skips INTEGER NOT NULL CHECK (skips > 0),
  last_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (task_id, predecessor)
);
UPDATE meta SET value='106' WHERE key='state_format_version';
