-- D5: catalog profile 5 (D4 host catalog 1.0.0) adapters. Existing profiles,
-- grants and catalogs are unchanged; this only admits new rows.
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_command_name_check;
ALTER TABLE playbook_effects ADD CHECK(command_name IN ('message.send','decision.request','context.read','work.ensure','work.change','operation.cancel'));
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_status_check;
ALTER TABLE playbook_effects ADD CHECK(status IN ('queued','refused','cancelled','accepted','pending','ensured','changed','conflict','observed','incomplete','expired','too_late','indeterminate'));
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3,4,5));
CREATE TABLE playbook_work_keys (
 namespace_id uuid NOT NULL,
 generation bigint NOT NULL,
 logical_key text NOT NULL CHECK (octet_length(logical_key) BETWEEN 1 AND 4096),
 task_id uuid NOT NULL,
 request_digest text NOT NULL CHECK (octet_length(request_digest) BETWEEN 64 AND 128),
 created_at_ms bigint NOT NULL,
 PRIMARY KEY(namespace_id,generation,logical_key),
 UNIQUE(namespace_id,generation,task_id),
 FOREIGN KEY(namespace_id,generation) REFERENCES playbook_namespaces(namespace_id,generation)
);
UPDATE meta SET value='69' WHERE key='state_format_version';
