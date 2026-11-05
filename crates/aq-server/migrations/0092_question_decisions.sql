-- F5 agent questions. One typed `question` decision per (attempt, operation);
-- the answer targets the exact decision revision. The asking attempt waits on
-- it through an H4 HumanDecision wait; an answer that arrives after the
-- attempt ended is delivered as task-addressed mail for the continuation.
CREATE TABLE question_decisions (
 decision_id UUID PRIMARY KEY,
 attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
 operation_id UUID NOT NULL,
 node_id UUID NOT NULL,
 project_id UUID NOT NULL,
 workspace_id UUID NOT NULL,
 task_id UUID NOT NULL,
 session JSONB NOT NULL CHECK(octet_length(session::text)<=2048),
 attempt_generation BIGINT NOT NULL CHECK(attempt_generation>0),
 question TEXT NOT NULL CHECK(octet_length(question) BETWEEN 1 AND 4096),
 question_digest TEXT NOT NULL CHECK(octet_length(question_digest) BETWEEN 1 AND 128),
 subject_revision TEXT NOT NULL CHECK(octet_length(subject_revision) BETWEEN 1 AND 512),
 decision_revision BIGINT NOT NULL CHECK(decision_revision>0),
 state TEXT NOT NULL CHECK(state IN ('pending','answered')),
 expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>0),
 wait_id UUID NOT NULL,
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>=0),
 answer TEXT CHECK(octet_length(answer) BETWEEN 1 AND 16384),
 answered_by UUID REFERENCES users(id),
 answer_operation_id UUID,
 receipt_digest TEXT CHECK(octet_length(receipt_digest) BETWEEN 1 AND 128),
 answered_at_ms BIGINT CHECK(answered_at_ms>=0),
 delivery JSONB CHECK(octet_length(delivery::text)<=1024),
 UNIQUE(attempt_id,operation_id),
 CHECK((state='answered')=(answer IS NOT NULL AND answered_by IS NOT NULL AND answer_operation_id IS NOT NULL AND receipt_digest IS NOT NULL AND answered_at_ms IS NOT NULL AND delivery IS NOT NULL)),
 CHECK((state='pending')=(answer IS NULL AND answered_by IS NULL AND answer_operation_id IS NULL AND receipt_digest IS NULL AND answered_at_ms IS NULL AND delivery IS NULL))
);
CREATE UNIQUE INDEX question_decisions_answer_operation ON question_decisions(answered_by,answer_operation_id) WHERE answer_operation_id IS NOT NULL;
CREATE INDEX question_decisions_task ON question_decisions(project_id,task_id);
UPDATE meta SET value='92' WHERE key='state_format_version';
