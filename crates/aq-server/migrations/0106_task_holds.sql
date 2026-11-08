-- R6 (lessons W83, W87): a server-side decision that keeps a ready task from
-- being offered (an exhausted retry, time or cost budget, a quarantine, a stuck
-- successor) is recorded on the task once, so the owner sees why it is not
-- running. `reason` is a server reason code, never worker text. `comment_body`
-- is the fixed-template system comment `comment_id` posts (once; `commented`
-- once it is on record), built from `reason` when the hold is recorded.
--
-- A hold belongs to one attempt history (`attempts`, the task's attempt count
-- the deciding read saw; it is recorded only while the count is unchanged). It
-- is cleared when the task gets a new attempt, when the task's claim is
-- released (any release path: B4, owner disposition, settlement), or when AQ
-- observes the task closed or retired in Graph.
CREATE TABLE task_holds (
  comment_id UUID PRIMARY KEY,
  task_id UUID NOT NULL,
  attempts INTEGER NOT NULL CHECK (attempts > 0),
  project_id UUID NOT NULL,
  reason TEXT NOT NULL CHECK (reason ~ '^[a-z][a-z0-9_]{0,95}$'),
  attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
  comment_body TEXT NOT NULL CHECK (length(comment_body) BETWEEN 1 AND 2048),
  commented BOOLEAN NOT NULL DEFAULT FALSE,
  held_at TIMESTAMPTZ NOT NULL,
  -- The last Graph observation of the task by the hold sweep.
  checked_at TIMESTAMPTZ,
  cleared_at TIMESTAMPTZ,
  cleared_by TEXT CHECK (cleared_by IN ('new_attempt','claim_released','task_closed')),
  CHECK ((cleared_at IS NULL) = (cleared_by IS NULL))
);
CREATE UNIQUE INDEX task_holds_one_current ON task_holds(task_id) WHERE cleared_at IS NULL;
CREATE INDEX task_holds_current_project ON task_holds(project_id) WHERE cleared_at IS NULL;
-- The one definition of a held task: the task view and the swarm read both
-- read this view.
CREATE VIEW task_holds_current AS
  SELECT h.* FROM task_holds h WHERE h.cleared_at IS NULL
  AND h.attempts = (SELECT count(*) FROM execution_attempts a WHERE a.task_id = h.task_id);
CREATE FUNCTION task_holds_clear() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP = 'INSERT' THEN
  UPDATE task_holds SET cleared_at = now(), cleared_by = 'new_attempt'
   WHERE task_id = NEW.task_id AND cleared_at IS NULL;
 ELSIF OLD.holds_task AND NOT NEW.holds_task THEN
  UPDATE task_holds SET cleared_at = now(), cleared_by = 'claim_released'
   WHERE task_id = NEW.task_id AND cleared_at IS NULL;
 END IF;
 RETURN NULL;
END $$;
CREATE TRIGGER execution_attempts_clear_task_holds AFTER INSERT OR UPDATE OF holds_task
  ON execution_attempts FOR EACH ROW EXECUTE FUNCTION task_holds_clear();
-- Swarm phase 1 (#117) recorded a lost attempt's hold on its release and named
-- it in a system comment. Those holds become task holds, with that comment, so
-- the task is never commented twice.
INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,comment_body,commented,held_at)
SELECT COALESCE(c.comment_id, gen_random_uuid()), a.task_id,
  (SELECT count(*) FROM execution_attempts x WHERE x.task_id = a.task_id),
  (a.state#>>'{offer,manifest,bindings,project_id}')::UUID, l.held_reason, l.attempt_id,
  COALESCE(c.body, 'AQ: this task is held (hold reason: ' || l.held_reason
    || '). It is not offered again until an owner acts; an owner can retire the task.'),
  c.comment_id IS NOT NULL, l.recorded_at
FROM lost_attempt_releases l JOIN execution_attempts a USING (attempt_id)
LEFT JOIN LATERAL (
  SELECT t.comment_id, t.body FROM task_comments t
  WHERE t.task_id = a.task_id AND t.author_kind = 'system' AND t.body IS NOT NULL
  AND t.body LIKE 'AQ: this task is held. Its last attempt (' || l.attempt_id::TEXT || ')%'
  ORDER BY t.created_at LIMIT 1) c ON TRUE
WHERE l.held_reason ~ '^[a-z][a-z0-9_]{0,95}$' AND a.holds_task
  AND a.state#>>'{offer,manifest,bindings,project_id}' ~ '^[0-9a-f-]{36}$'
ON CONFLICT DO NOTHING;
-- Consecutive skips of one predecessor's successor (a refused handoff export or
-- selection) since `first_at`; a gap of five minutes starts the count again.
CREATE TABLE successor_skips (
  task_id UUID NOT NULL,
  predecessor UUID NOT NULL REFERENCES execution_attempts(attempt_id),
  skips INTEGER NOT NULL CHECK (skips > 0),
  first_at TIMESTAMPTZ NOT NULL,
  last_at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (task_id, predecessor)
);
UPDATE meta SET value='106' WHERE key='state_format_version';
