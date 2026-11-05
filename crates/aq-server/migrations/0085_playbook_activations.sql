-- D9 owner artifact operations ledger. One row per caller operation id per
-- playbook namespace: a replay returns the recorded row. Activate/rollback rows
-- for a binding in seq order form the activation stack used by rollback.
CREATE TABLE playbook_activations (
 namespace_id UUID NOT NULL REFERENCES playbook_namespaces(namespace_id),
 op_id TEXT NOT NULL CHECK(char_length(op_id) BETWEEN 1 AND 128),
 seq BIGSERIAL NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN ('install','activate','rollback')),
 binding TEXT NOT NULL CHECK(char_length(binding) BETWEEN 1 AND 128),
 artifact TEXT NOT NULL CHECK(char_length(artifact) BETWEEN 1 AND 128),
 previous TEXT CHECK(char_length(previous) BETWEEN 1 AND 128),
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>=0),
 PRIMARY KEY(namespace_id,op_id)
);
CREATE INDEX playbook_activations_binding ON playbook_activations(namespace_id,binding,seq);
UPDATE meta SET value='85' WHERE key='state_format_version';
