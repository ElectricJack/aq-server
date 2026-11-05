-- D8 adds a new namespace profile; immutable old catalogs are never restamped.
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3,4,5,6));
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_command_name_check;
ALTER TABLE playbook_effects ADD CHECK(command_name IN ('message.send','decision.request','context.read','work.ensure','work.change','operation.cancel','findings.reconcile'));
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_status_check;
ALTER TABLE playbook_effects ADD CHECK(status IN ('queued','refused','cancelled','accepted','pending','ensured','changed','conflict','observed','incomplete','expired','too_late','indeterminate','reconciled'));
-- Event identity is producer + event_id within a namespace, never event_id alone.
ALTER TABLE playbook_outbox DROP CONSTRAINT playbook_outbox_pkey;
ALTER TABLE playbook_outbox ADD PRIMARY KEY(namespace_id,generation,producer,event_id);
CREATE TABLE playbook_schedules (
 namespace_id uuid NOT NULL, generation bigint NOT NULL, schedule_id uuid NOT NULL,
 revision bigint NOT NULL CHECK(revision > 0), definition jsonb NOT NULL CHECK(octet_length(definition::text)<=4096),
 cursor_date date NOT NULL, created_at_ms bigint NOT NULL,
 PRIMARY KEY(namespace_id,generation,schedule_id),
 FOREIGN KEY(namespace_id,generation) REFERENCES playbook_namespaces(namespace_id,generation)
);
CREATE TABLE playbook_schedule_revisions (
 namespace_id uuid NOT NULL, generation bigint NOT NULL, schedule_id uuid NOT NULL,
 revision bigint NOT NULL, definition jsonb NOT NULL CHECK(octet_length(definition::text)<=4096),
 PRIMARY KEY(namespace_id,generation,schedule_id,revision),
 FOREIGN KEY(namespace_id,generation,schedule_id) REFERENCES playbook_schedules(namespace_id,generation,schedule_id)
);
CREATE TABLE playbook_health_observations (
 namespace_id uuid NOT NULL, generation bigint NOT NULL, observation text NOT NULL CHECK(octet_length(observation) BETWEEN 1 AND 512),
 partition text NOT NULL CHECK(octet_length(partition) BETWEEN 1 AND 512), baseline jsonb NOT NULL CHECK(octet_length(baseline::text)<=65536),
 expires_at_ms bigint NOT NULL, evaluation text, complete boolean NOT NULL DEFAULT false,
 request_digest text, result jsonb,
 PRIMARY KEY(namespace_id,generation,observation),
 FOREIGN KEY(namespace_id,generation) REFERENCES playbook_namespaces(namespace_id,generation)
);
CREATE TABLE playbook_finding_episodes (
 namespace_id uuid NOT NULL, generation bigint NOT NULL, partition text NOT NULL CHECK(octet_length(partition) BETWEEN 1 AND 512), finding_key text NOT NULL CHECK(octet_length(finding_key) BETWEEN 1 AND 512),
 revision bigint NOT NULL CHECK(revision>0), present boolean NOT NULL,
 evidence text NOT NULL CHECK(octet_length(evidence)<=4096), last_notified_ms bigint NOT NULL,
 PRIMARY KEY(namespace_id,generation,partition,finding_key),
 FOREIGN KEY(namespace_id,generation) REFERENCES playbook_namespaces(namespace_id,generation)
);
CREATE TABLE playbook_finding_notifications (
 sequence bigint GENERATED ALWAYS AS IDENTITY,
 namespace_id uuid NOT NULL, generation bigint NOT NULL, notification_id uuid NOT NULL,
 request jsonb NOT NULL, submitted boolean NOT NULL DEFAULT false,
 PRIMARY KEY(namespace_id,generation,notification_id),
 FOREIGN KEY(namespace_id,generation) REFERENCES playbook_namespaces(namespace_id,generation)
);
CREATE INDEX playbook_finding_notifications_pending ON playbook_finding_notifications(namespace_id,generation,sequence) WHERE NOT submitted;
UPDATE meta SET value='70' WHERE key='state_format_version';
