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

UPDATE meta SET value='182' WHERE key='state_format_version';
