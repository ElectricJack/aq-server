-- D7: catalog profile 7 integration adapters candidate.prepare and
-- candidate.publish. Existing rows are unchanged; this only admits new rows.
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_command_name_check;
ALTER TABLE playbook_effects ADD CHECK(command_name IN ('message.send','decision.request','context.read','work.ensure','work.change','operation.cancel','findings.reconcile','judgment.request','check.request','candidate.prepare','candidate.publish'));
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_status_check;
ALTER TABLE playbook_effects ADD CHECK(status IN ('queued','refused','cancelled','accepted','pending','ensured','changed','conflict','observed','incomplete','expired','too_late','indeterminate','reconciled','prepared','published'));
-- One receipt per candidate or publication operation, recorded `pending`
-- with the playbook_effects intent before the delivery service is called.
-- `ambiguous` is a publication dispatched without acknowledgement; it only
-- ever settles by reconciliation, never by a second publish. `request` and
-- `detail` hold bounded identifiers, OIDs and typed reasons, never
-- credential material. A settled receipt is never rewritten.
CREATE TABLE playbook_integration_receipts (
 operation_id uuid PRIMARY KEY,
 namespace_id uuid NOT NULL,
 generation bigint NOT NULL,
 intent_id text NOT NULL CHECK (octet_length(intent_id) BETWEEN 1 AND 512),
 command_name text NOT NULL CHECK (command_name IN ('candidate.prepare','candidate.publish')),
 candidate_operation_id uuid NOT NULL,
 repository_id uuid,
 binding_generation bigint CHECK (binding_generation IS NULL OR binding_generation >= 1),
 state text NOT NULL CHECK (state IN ('pending','ambiguous','prepared','conflict','refused','published')),
 reason text CHECK (reason IS NULL OR octet_length(reason) BETWEEN 1 AND 128),
 request jsonb NOT NULL CHECK (octet_length(request::text) <= 65536),
 detail jsonb CHECK (detail IS NULL OR octet_length(detail::text) <= 65536),
 reconcile_attempts integer NOT NULL DEFAULT 0 CHECK (reconcile_attempts BETWEEN 0 AND 64),
 created_at_ms bigint NOT NULL,
 settled_at_ms bigint,
 CHECK ((state IN ('pending','ambiguous')) = (settled_at_ms IS NULL)),
 CHECK ((command_name = 'candidate.prepare') = (operation_id = candidate_operation_id)),
 UNIQUE(namespace_id,generation,intent_id),
 FOREIGN KEY(namespace_id,generation) REFERENCES playbook_namespaces(namespace_id,generation)
);
CREATE INDEX playbook_integration_receipts_candidate ON playbook_integration_receipts(namespace_id,generation,candidate_operation_id);
UPDATE meta SET value='80' WHERE key='state_format_version';
