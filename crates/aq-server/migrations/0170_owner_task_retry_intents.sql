-- Keep approved task retries resumable from the original dashboard operation.
-- Requests admitted before this migration lack the information needed to replay.
CREATE TABLE owner_task_retry_intents (
 operation_id UUID PRIMARY KEY REFERENCES dashboard_operations(operation_id),
 project_id UUID NOT NULL,
 project_revision BIGINT NOT NULL CHECK(project_revision > 0),
 task_id UUID NOT NULL,
 predecessor UUID REFERENCES execution_attempts(attempt_id),
 request JSONB NOT NULL,
 next_check_at TIMESTAMPTZ NOT NULL,
 checks INTEGER NOT NULL DEFAULT 0 CHECK(checks BETWEEN 0 AND 7),
 released_at TIMESTAMPTZ,
 CHECK(released_at IS NULL OR predecessor IS NOT NULL)
);

CREATE INDEX owner_task_retry_due ON owner_task_retry_intents(next_check_at,operation_id);

CREATE FUNCTION guard_owner_task_retry_intent() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP <> 'UPDATE' OR
    (to_jsonb(NEW)-ARRAY['next_check_at','checks','released_at']) IS DISTINCT FROM
      (to_jsonb(OLD)-ARRAY['next_check_at','checks','released_at']) OR
    (OLD.released_at IS NOT NULL AND NEW.released_at IS DISTINCT FROM OLD.released_at)
 THEN
  RAISE EXCEPTION 'owner task retry intent and release evidence are immutable';
 END IF;
 RETURN NEW;
END $$;

CREATE TRIGGER owner_task_retry_intent_guard BEFORE UPDATE OR DELETE ON owner_task_retry_intents
 FOR EACH ROW EXECUTE FUNCTION guard_owner_task_retry_intent();
CREATE TRIGGER owner_task_retry_intent_truncate_guard BEFORE TRUNCATE ON owner_task_retry_intents
 FOR EACH STATEMENT EXECUTE FUNCTION guard_owner_task_retry_intent();
UPDATE meta SET value='170' WHERE key='state_format_version';
