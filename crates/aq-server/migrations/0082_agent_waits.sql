-- H4: durable agent waits. One reducer snapshot per wait; outcomes and resume
-- intents commit together and are immutable once recorded.
CREATE TABLE agent_waits (
 wait_id UUID PRIMARY KEY,
 caller_key TEXT NOT NULL,
 operation_id UUID NOT NULL,
 project_id UUID NOT NULL,
 workspace_id UUID NOT NULL,
 task_id UUID NOT NULL,
 attempt_id UUID NOT NULL,
 node_id UUID NOT NULL,
 wait_generation BIGINT NOT NULL CHECK (wait_generation > 0),
 continuation_generation BIGINT NOT NULL CHECK (continuation_generation > 0),
 predicate_digest TEXT NOT NULL,
 request_digest TEXT NOT NULL,
 phase TEXT NOT NULL CHECK (phase IN ('installing','pending','recovery_held','terminal')),
 revision BIGINT NOT NULL CHECK (revision > 0),
 next_due_ms BIGINT NOT NULL,
 deadline_ms BIGINT NOT NULL,
 created_at_ms BIGINT NOT NULL,
 snapshot BYTEA NOT NULL,
 response JSONB NOT NULL,
 UNIQUE (caller_key, operation_id)
);
-- At most one active (non-terminal) wait per attempt.
CREATE UNIQUE INDEX agent_waits_one_active_per_attempt ON agent_waits (attempt_id) WHERE phase <> 'terminal';
CREATE INDEX agent_waits_due ON agent_waits (next_due_ms) WHERE phase <> 'terminal';
CREATE INDEX agent_waits_project ON agent_waits (project_id, task_id);

CREATE TABLE agent_wait_outcomes (
 wait_id UUID PRIMARY KEY REFERENCES agent_waits(wait_id),
 outcome_id UUID NOT NULL UNIQUE,
 kind TEXT NOT NULL,
 outcome JSONB NOT NULL,
 digest TEXT NOT NULL,
 recorded_at_ms BIGINT NOT NULL
);
CREATE FUNCTION agent_wait_immutable_record() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'immutable agent wait record'; END $$;
CREATE TRIGGER agent_wait_outcomes_immutable BEFORE UPDATE OR DELETE ON agent_wait_outcomes
 FOR EACH ROW EXECUTE FUNCTION agent_wait_immutable_record();

CREATE TABLE agent_wait_resume_intents (
 intent_id UUID PRIMARY KEY,
 wait_id UUID NOT NULL UNIQUE REFERENCES agent_waits(wait_id),
 continuation_id UUID NOT NULL,
 continuation_generation BIGINT NOT NULL,
 outcome_id UUID NOT NULL REFERENCES agent_wait_outcomes(outcome_id),
 state TEXT NOT NULL CHECK (state IN ('ready','consumed','retired')),
 admission_id UUID
);

CREATE TABLE agent_wait_acks (
 admission_id UUID PRIMARY KEY,
 intent_id UUID NOT NULL UNIQUE REFERENCES agent_wait_resume_intents(intent_id),
 request_digest TEXT NOT NULL,
 response JSONB NOT NULL,
 created_at_ms BIGINT NOT NULL
);
CREATE TRIGGER agent_wait_acks_immutable BEFORE UPDATE OR DELETE ON agent_wait_acks
 FOR EACH ROW EXECUTE FUNCTION agent_wait_immutable_record();
UPDATE meta SET value='82' WHERE key='state_format_version';
