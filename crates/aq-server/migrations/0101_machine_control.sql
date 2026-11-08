-- Spec 0007 phase 1: a service machine is the set of live nodes sharing a
-- machine key (the worker-machine/v1 key, claimed by slot 1); the owner's
-- desired state per machine; handoff requests; renewed context facts; and the
-- owner's sticky availability switch.
ALTER TABLE nodes ADD COLUMN machine_key TEXT CHECK (machine_key ~ '^[a-z0-9][a-z0-9-]{0,62}$');
ALTER TABLE nodes ADD COLUMN slot INTEGER CHECK (slot BETWEEN 1 AND 8);
ALTER TABLE nodes ADD CONSTRAINT nodes_machine_slot_pair CHECK ((machine_key IS NULL) = (slot IS NULL));
CREATE UNIQUE INDEX nodes_live_machine_slot ON nodes(machine_key, slot)
  WHERE machine_key IS NOT NULL AND revoked_at IS NULL;

CREATE TABLE machine_controls (
  machine_key TEXT PRIMARY KEY CHECK (machine_key ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
  desired TEXT NOT NULL CHECK (desired IN ('running','draining','stopped')),
  generation BIGINT NOT NULL CHECK (generation >= 2),
  operation_id UUID NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL,
  updated_by UUID NOT NULL REFERENCES users(id)
);

CREATE TABLE attempt_handoffs (
  attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id),
  node_id UUID NOT NULL REFERENCES nodes(id),
  reason TEXT NOT NULL CHECK (reason IN ('owner_stop','service_stop','restart')),
  requested_at TIMESTAMPTZ NOT NULL,
  candidate_operation_id UUID,
  candidate_oid TEXT CHECK (candidate_oid ~ '^[0-9a-f]{40}$'),
  note_comment_id UUID
);
CREATE INDEX attempt_handoffs_node ON attempt_handoffs(node_id);

CREATE TABLE context_qualification_renewals (
  renderer_digest TEXT PRIMARY KEY CHECK (renderer_digest ~ '^[0-9a-f]{64}$'),
  base_renderer_digest TEXT NOT NULL CHECK (base_renderer_digest ~ '^[0-9a-f]{64}$'),
  fact JSONB NOT NULL,
  observed_at_ms BIGINT NOT NULL,
  expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms > observed_at_ms AND expires_at_ms - observed_at_ms <= 86400000),
  node_id UUID NOT NULL REFERENCES nodes(id),
  submission JSONB NOT NULL CHECK (octet_length(submission::text) <= 262144),
  created_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX context_qualification_renewals_expiry ON context_qualification_renewals(expires_at_ms DESC);

CREATE TABLE owner_availability (
  singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
  available BOOLEAN NOT NULL,
  generation BIGINT NOT NULL CHECK (generation >= 2),
  operation_id UUID NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL,
  updated_by UUID NOT NULL REFERENCES users(id)
);
UPDATE meta SET value='101' WHERE key='state_format_version';
