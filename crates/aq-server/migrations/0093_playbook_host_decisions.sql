-- D11: catalog profile 7 decision.request is asynchronous. One question is
-- tracked as one running host operation and completes exactly once
-- (aq.decision.completed approved/rejected/expired) by a listed responder's
-- answer or its expiry. Existing rows are unchanged; this only admits new rows.
ALTER TABLE playbook_host_operations DROP CONSTRAINT playbook_host_operations_command_name_check;
ALTER TABLE playbook_host_operations ADD CHECK (command_name IN ('judgment.request','check.request','work.ensure','decision.request'));
ALTER TABLE playbook_host_operations DROP CONSTRAINT playbook_host_operations_status_check;
ALTER TABLE playbook_host_operations ADD CHECK (status IN ('running','assessed','failed','refused','passed','infrastructure_failure','completed','approved','rejected','expired'));
UPDATE meta SET value='93' WHERE key='state_format_version';
