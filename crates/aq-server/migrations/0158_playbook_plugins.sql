ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_command_name_check;
ALTER TABLE playbook_effects ADD CHECK (
    command_name ~ '^ext\.[a-z][a-z0-9_]{1,31}\..+$'
    OR command_name IN ('message.send','decision.request','context.read','work.ensure','work.change','operation.cancel','findings.reconcile','judgment.request','check.request','candidate.prepare','candidate.publish','work.read','work.route','work.unroutable','judgment.assess','work.complete','work.return','work.hold','supervisor.notify','main.sync','branch.publish','pull_request.open','pull_request.await_checks','pull_request.merge','pull_request.close')
);
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_status_check;
ALTER TABLE playbook_effects ADD CONSTRAINT playbook_effects_status_check CHECK (
    (status = 'plugin_completed' AND command_name ~ '^ext\.')
    OR (command_name !~ '^ext\.' AND status IN ('queued','refused','cancelled','accepted','pending','ensured','changed','conflict','observed','incomplete','expired','too_late','indeterminate','reconciled','prepared','published','routed','stale','recorded','completed','returned','held','sent','duplicate'))
);
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_receipt_check;
ALTER TABLE playbook_effects ADD CHECK (
    octet_length(receipt::text) <= CASE WHEN status = 'plugin_completed' THEN 2097152 ELSE 16384 END
);

ALTER TABLE playbook_outbox ADD COLUMN plugin_event_id TEXT,
    ADD COLUMN plugin_retain_until_ms BIGINT;
ALTER TABLE playbook_outbox DROP CONSTRAINT playbook_outbox_producer_check;
ALTER TABLE playbook_outbox ADD CHECK (
    (producer ~ '^ext\.[a-z][a-z0-9_]{1,31}$' AND plugin_event_id IS NOT NULL
        AND plugin_event_id ~ '^sha256:[a-f0-9]{64}$' AND plugin_retain_until_ms IS NOT NULL)
    OR (producer IN ('aq.host.bridge@1','aq.human.decisions@1','aq.graph.operations@1','aq.task.settlement@1')
        AND plugin_event_id IS NULL AND plugin_retain_until_ms IS NULL)
);

CREATE TABLE playbook_plugin_notifications (
    identity TEXT PRIMARY KEY,
    namespace_id UUID NOT NULL,
    generation BIGINT NOT NULL,
    request JSONB NOT NULL,
    created_at_ms BIGINT NOT NULL,
    submitted BOOLEAN NOT NULL DEFAULT FALSE,
    FOREIGN KEY(namespace_id, generation) REFERENCES playbook_namespaces(namespace_id, generation)
);
CREATE INDEX playbook_plugin_notifications_pending
    ON playbook_plugin_notifications(namespace_id, generation, created_at_ms)
    WHERE NOT submitted;

UPDATE meta SET value='158' WHERE key='state_format_version';
