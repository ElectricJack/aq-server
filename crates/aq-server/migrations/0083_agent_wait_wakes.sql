-- H6 hibernation wake scheduling. The server side of suspension: a directive,
-- an accepted checkpoint, and a parking report whose `suspended` disposition is
-- neither success nor failure (no result report, no settlement input).
CREATE TABLE agent_wait_suspensions (
 attempt_id UUID PRIMARY KEY,
 wait_id UUID NOT NULL UNIQUE REFERENCES agent_waits(wait_id),
 directive JSONB NOT NULL CHECK(octet_length(directive::TEXT)<=16384),
 checkpoint JSONB CHECK(checkpoint IS NULL OR octet_length(checkpoint::TEXT)<=65536),
 report JSONB CHECK(report IS NULL OR octet_length(report::TEXT)<=65536),
 disposition TEXT NOT NULL CHECK(disposition IN ('directed','checkpointed','held','suspended')),
 CHECK((disposition='suspended')=(report IS NOT NULL AND report#>>'{disposition,kind}'='retained_suspended')),
 updated_at_ms BIGINT NOT NULL CHECK(updated_at_ms>0)
);
-- One scheduler projection per H4 resume intent. `resume_pending` is never a
-- running state: only an admission winner is. Age comes from the wait and is
-- never reset by an early resume or a deadline extension.
CREATE TABLE agent_wait_wakes (
 intent_id UUID PRIMARY KEY REFERENCES agent_wait_resume_intents(intent_id),
 wait_id UUID NOT NULL UNIQUE REFERENCES agent_waits(wait_id),
 project_id UUID NOT NULL,
 task_id UUID NOT NULL,
 predecessor_attempt_id UUID NOT NULL,
 predecessor_node_id UUID,
 dispatch_generation BIGINT NOT NULL CHECK(dispatch_generation>0),
 state TEXT NOT NULL CHECK(state IN ('resume_pending','admitted','acknowledged','escalated','cancelled','superseded')),
 reason TEXT CHECK(reason IS NULL OR octet_length(reason) BETWEEN 1 AND 256),
 launch_failures INTEGER NOT NULL DEFAULT 0 CHECK(launch_failures BETWEEN 0 AND 3),
 holds INTEGER NOT NULL DEFAULT 0 CHECK(holds>=0),
 manual_override BOOLEAN NOT NULL DEFAULT FALSE,
 wait_created_at_ms BIGINT NOT NULL CHECK(wait_created_at_ms>0),
 ready_at_ms BIGINT NOT NULL CHECK(ready_at_ms>0),
 next_due_ms BIGINT NOT NULL,
 updated_at_ms BIGINT NOT NULL CHECK(updated_at_ms>0)
);
CREATE INDEX agent_wait_wakes_due ON agent_wait_wakes(next_due_ms,ready_at_ms) WHERE state='resume_pending';
-- Exactly one durable winner per dispatch generation. A retry reads it back.
CREATE TABLE agent_wait_wake_admissions (
 admission_id UUID PRIMARY KEY,
 intent_id UUID NOT NULL REFERENCES agent_wait_wakes(intent_id),
 dispatch_generation BIGINT NOT NULL CHECK(dispatch_generation>0),
 node_id UUID REFERENCES nodes(id),
 central BOOLEAN NOT NULL,
 CHECK(central=(node_id IS NULL)),
 successor_attempt_id UUID NOT NULL UNIQUE,
 successor_session_id UUID NOT NULL UNIQUE,
 attachment JSONB NOT NULL CHECK(octet_length(attachment::TEXT)<=262144),
 attachment_digest TEXT NOT NULL,
 differences JSONB NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('offered','acknowledged','failed')),
 failure TEXT CHECK(failure IS NULL OR octet_length(failure) BETWEEN 1 AND 256),
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>0),
 UNIQUE(intent_id,dispatch_generation)
);
UPDATE meta SET value='83' WHERE key='state_format_version';
