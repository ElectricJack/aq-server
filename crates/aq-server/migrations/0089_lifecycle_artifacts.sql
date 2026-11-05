-- H5 follow-up (crisp-ember-25): arbitrary-artifact retention with lifecycle
-- RetentionReceipts, AQ-issued ExecutionPermits, and a generic idempotent
-- operation ledger for the lifecycle surfaces.
--
-- Retained artifacts are bound to the lifecycle Binding of the uploading
-- attempt. A successor on the same (project, task, lineage) may fetch them.
CREATE TABLE retained_artifacts (
 transfer_id UUID PRIMARY KEY,
 receipt_id UUID NOT NULL UNIQUE,
 node_id UUID NOT NULL REFERENCES nodes(id),
 attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
 project_id UUID NOT NULL,
 task_id UUID NOT NULL,
 lineage_id UUID NOT NULL,
 binding JSONB NOT NULL,
 binding_digest TEXT NOT NULL,
 role TEXT NOT NULL CHECK(role IN ('source','checkpoint','transcript','result')),
 artifact JSONB NOT NULL,
 declared_bytes BIGINT NOT NULL,
 upload_offset BIGINT NOT NULL DEFAULT 0,
 state TEXT NOT NULL DEFAULT 'uploading' CHECK(state IN ('uploading','retained','rejected')),
 receipt JSONB,
 retained_at_ms BIGINT,
 retained_until_ms BIGINT,
 released BOOLEAN NOT NULL DEFAULT FALSE,
 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX retained_artifacts_owner ON retained_artifacts(node_id,attempt_id);
CREATE INDEX retained_artifacts_lineage ON retained_artifacts(project_id,task_id,lineage_id) WHERE NOT released;

-- Idempotent operation ledger shared by the lifecycle surfaces (artifacts,
-- permits). Same shape as source_transfer_operations; the response replays
-- byte-for-byte and a changed payload is a conflict.
CREATE TABLE execution_operations (
 node_id UUID NOT NULL REFERENCES nodes(id),
 operation_id UUID NOT NULL,
 payload_digest TEXT NOT NULL,
 response JSONB NOT NULL,
 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 PRIMARY KEY(node_id,operation_id)
);

-- AQ-issued ExecutionPermits (aq-session-host 0.1.7). One monotonic sequence
-- per attempt; the manifest digest is pinned by the first permit when the
-- attempt record carries no launch manifest digest.
CREATE TABLE execution_permits (
 permit_id UUID PRIMARY KEY,
 attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
 node_id UUID NOT NULL REFERENCES nodes(id),
 sequence BIGINT NOT NULL,
 manifest_digest TEXT NOT NULL,
 permit JSONB NOT NULL,
 issued_at_ms BIGINT NOT NULL,
 UNIQUE(attempt_id,sequence)
);
UPDATE meta SET value='89' WHERE key='state_format_version';
