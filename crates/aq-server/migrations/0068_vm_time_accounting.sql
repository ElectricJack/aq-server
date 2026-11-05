-- Additive ledger: old token-call facts and backend advertisements are unchanged.
CREATE TABLE accounting_vm_time (
 node_id UUID NOT NULL REFERENCES nodes(id),
 attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id),
 sandbox_id TEXT NOT NULL CHECK(length(sandbox_id) BETWEEN 1 AND 256),
 started_at_ms BIGINT NOT NULL CHECK(started_at_ms>0),
 stopped_at_ms BIGINT CHECK(stopped_at_ms>=started_at_ms),
 fact JSONB NOT NULL,
 rate_card JSONB NOT NULL,
 PRIMARY KEY(node_id,sandbox_id),
 UNIQUE(attempt_id),
 CHECK(octet_length(fact::TEXT)<=2048),
 CHECK(octet_length(rate_card::TEXT)<=4096)
);
CREATE INDEX accounting_vm_time_month ON accounting_vm_time(node_id,stopped_at_ms);
UPDATE meta SET value='68' WHERE key='state_format_version';
