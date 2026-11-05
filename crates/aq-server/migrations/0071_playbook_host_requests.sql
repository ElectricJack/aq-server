-- D6: catalog profile 7 (AQ host catalog 2.0.0) asynchronous provider
-- judgments and sandbox checks. Existing profiles, grants, catalogs and rows
-- are unchanged; this only admits new rows.
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_command_name_check;
ALTER TABLE playbook_effects ADD CHECK(command_name IN ('message.send','decision.request','context.read','work.ensure','work.change','operation.cancel','findings.reconcile','judgment.request','check.request'));
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3,4,5,6,7));
-- One durable intent per admitted judgment/check operation, recorded before
-- any provider request or sandbox start. `request` holds digests and bounded
-- inputs only, never credential material. `output` is the single retained
-- completion; a completed row is never rewritten.
CREATE TABLE playbook_host_operations (
 operation_id uuid PRIMARY KEY,
 namespace_id uuid NOT NULL,
 generation bigint NOT NULL,
 intent_id text NOT NULL CHECK (octet_length(intent_id) BETWEEN 1 AND 512),
 command_name text NOT NULL CHECK (command_name IN ('judgment.request','check.request')),
 subject_id text NOT NULL CHECK (octet_length(subject_id) BETWEEN 1 AND 4096),
 subject_revision bigint NOT NULL CHECK (subject_revision >= 0),
 check_id text CHECK (check_id IS NULL OR octet_length(check_id) BETWEEN 1 AND 512),
 request jsonb NOT NULL CHECK (octet_length(request::text) <= 131072),
 status text NOT NULL CHECK (status IN ('running','assessed','failed','refused','passed','infrastructure_failure')),
 output jsonb CHECK (output IS NULL OR octet_length(output::text) <= 65536),
 created_at_ms bigint NOT NULL,
 completed_at_ms bigint,
 CHECK ((status = 'running') = (output IS NULL)),
 CHECK ((command_name = 'check.request') = (check_id IS NOT NULL)),
 UNIQUE(namespace_id,generation,intent_id),
 FOREIGN KEY(namespace_id,generation) REFERENCES playbook_namespaces(namespace_id,generation)
);
CREATE INDEX playbook_host_operations_subject ON playbook_host_operations(namespace_id,generation,subject_id,check_id);
UPDATE meta SET value='71' WHERE key='state_format_version';
