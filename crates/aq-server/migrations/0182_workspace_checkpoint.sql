-- Preserve the checkpoint cause separately from the legacy worker stop wire.
ALTER TABLE attempt_handoffs DROP CONSTRAINT attempt_handoffs_reason_check;
ALTER TABLE attempt_handoffs ADD CONSTRAINT attempt_handoffs_reason_check
    CHECK (reason IN ('owner_stop','service_stop','restart','workspace_expansion'));

-- Freeze the authority of a workspace-capable offer so an approval or edit
-- racing an in-flight preparation cannot change its workspace.
CREATE TABLE task_workspace_offer_fences (
    attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
    scope JSONB NOT NULL CHECK (jsonb_typeof(scope) = 'object')
);

CREATE INDEX task_workspace_sources_requester
    ON task_workspace_sources(requested_attempt_id) WHERE requested_attempt_id IS NOT NULL;

CREATE INDEX task_workspace_services_requester
    ON task_workspace_services(requested_attempt_id) WHERE requested_attempt_id IS NOT NULL;

-- The request and checkpoint intent share a transaction. A machine poll
-- sees only committed handoffs and keeps redelivering while the attempt runs.
CREATE FUNCTION task_workspace_request_checkpoint() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at)
    SELECT a.attempt_id,a.node_id,'workspace_expansion',clock_timestamp()
    FROM execution_attempts a
    JOIN profiled_launches l ON l.attempt_id=a.attempt_id
    WHERE a.attempt_id=NEW.requested_attempt_id
      AND a.kind='profiled_v1' AND a.holds_task AND a.task_id=NEW.task_id
      AND l.launch_manifest#>>'{bindings,project_id}'=NEW.home_project_id::text
    ON CONFLICT(attempt_id) DO NOTHING;
    RETURN NULL;
END $$;

CREATE TRIGGER task_workspace_source_checkpoint
    AFTER INSERT OR UPDATE OF requested_attempt_id ON task_workspace_sources
    FOR EACH ROW WHEN (NEW.requested_attempt_id IS NOT NULL)
    EXECUTE FUNCTION task_workspace_request_checkpoint();

CREATE TRIGGER task_workspace_service_checkpoint
    AFTER INSERT OR UPDATE OF requested_attempt_id ON task_workspace_services
    FOR EACH ROW WHEN (NEW.requested_attempt_id IS NOT NULL)
    EXECUTE FUNCTION task_workspace_request_checkpoint();

UPDATE meta SET value='182' WHERE key='state_format_version';
