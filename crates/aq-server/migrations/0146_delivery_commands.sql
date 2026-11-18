-- Epic 0, delivery by playbook (spec 0016 section 6.2): the six asynchronous
-- commands of host catalog 2.3.0 (`main.sync`, `branch.publish`,
-- `pull_request.open`, `pull_request.await_checks`, `pull_request.merge` and
-- `pull_request.close`) are admitted on the bridge tables. Each is one running
-- host operation that completes exactly once, by `aq.delivery.completed`; its
-- completion outcome is the operation's status. Additive: existing rows are
-- untouched, so an older server keeps running on the migrated schema. The
-- command list of `playbook_effects` keeps what 0144 added; its status list
-- (0144) already holds every status these commands record.
SET LOCAL lock_timeout = '5s';
ALTER TABLE playbook_effects DROP CONSTRAINT playbook_effects_command_name_check;
ALTER TABLE playbook_effects ADD CHECK(command_name IN ('message.send','decision.request','context.read','work.ensure','work.change','operation.cancel','findings.reconcile','judgment.request','check.request','candidate.prepare','candidate.publish','work.read','work.route','work.unroutable','judgment.assess','work.complete','work.return','work.hold','supervisor.notify','main.sync','branch.publish','pull_request.open','pull_request.await_checks','pull_request.merge','pull_request.close'));
ALTER TABLE playbook_host_operations DROP CONSTRAINT playbook_host_operations_command_name_check;
ALTER TABLE playbook_host_operations ADD CHECK (command_name IN ('judgment.request','check.request','work.ensure','decision.request','judgment.assess','main.sync','branch.publish','pull_request.open','pull_request.await_checks','pull_request.merge','pull_request.close'));
ALTER TABLE playbook_host_operations DROP CONSTRAINT playbook_host_operations_status_check;
ALTER TABLE playbook_host_operations ADD CHECK (status IN ('running','assessed','failed','refused','passed','infrastructure_failure','completed','approved','rejected','expired','synced','published','conflict','opened','closed','timeout','merged','not_mergeable'));
-- Running delivery operations, in the order they were last looked at, for the
-- driver's bounded pass over them.
CREATE INDEX playbook_host_operations_running_delivery ON playbook_host_operations(namespace_id,generation,(COALESCE(observed_at_ms,created_at_ms)))
 WHERE status='running' AND command_name IN ('main.sync','branch.publish','pull_request.open','pull_request.await_checks','pull_request.merge','pull_request.close');
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='146' WHERE key='state_format_version';
