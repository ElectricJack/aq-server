-- Host catalog 2.4.0 has a new immutable profile and a retained activation outcome.
SET LOCAL lock_timeout = '5s';
ALTER TABLE playbook_namespaces DROP CONSTRAINT playbook_namespaces_catalog_profile_check;
ALTER TABLE playbook_namespaces ADD CHECK(catalog_profile IN (1,2,3,4,5,6,7,8,9,10,11));

ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_command_name_check;
ALTER TABLE playbook_effects ADD CHECK (
    command_name ~ '^ext\.[a-z][a-z0-9_]{1,31}\..+$'
    OR command_name IN (
        'message.send','decision.request','context.read','work.ensure','work.change',
        'operation.cancel','findings.reconcile','judgment.request','check.request',
        'candidate.prepare','candidate.publish','work.read','work.route','work.unroutable',
        'judgment.assess','work.complete','work.return','work.hold','supervisor.notify',
        'main.sync','branch.publish','pull_request.open','pull_request.await_checks',
        'pull_request.merge','pull_request.close','work.activate'
    )
);
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_status_check;
ALTER TABLE playbook_effects ADD CONSTRAINT playbook_effects_status_check CHECK (
    (status = 'plugin_completed' AND command_name ~ '^ext\.')
    OR (status = 'activated' AND command_name = 'work.activate')
    OR (command_name !~ '^ext\.' AND status IN (
        'queued','refused','cancelled','accepted','pending','ensured','changed','conflict',
        'observed','incomplete','expired','too_late','indeterminate','reconciled','prepared',
        'published','routed','stale','recorded','completed','returned','held','sent','duplicate'
    ))
);
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='159' WHERE key='state_format_version';
