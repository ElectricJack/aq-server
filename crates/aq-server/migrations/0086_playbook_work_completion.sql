-- D9 prerequisite: catalog profile 7 work.ensure is asynchronous. An ensured
-- work node is tracked as one running host operation and completes exactly
-- once (aq.operation.updated completed/refused) when the graph task reaches a
-- terminal state. Existing rows are unchanged; this only admits new rows.
ALTER TABLE playbook_host_operations DROP CONSTRAINT playbook_host_operations_command_name_check;
ALTER TABLE playbook_host_operations ADD CHECK (command_name IN ('judgment.request','check.request','work.ensure'));
ALTER TABLE playbook_host_operations DROP CONSTRAINT playbook_host_operations_status_check;
ALTER TABLE playbook_host_operations ADD CHECK (status IN ('running','assessed','failed','refused','passed','infrastructure_failure','completed'));
-- Round-robin cursor for the work-completion collector.
ALTER TABLE playbook_host_operations ADD COLUMN observed_at_ms bigint;
CREATE INDEX playbook_host_operations_running_work ON playbook_host_operations(namespace_id,generation,(COALESCE(observed_at_ms,created_at_ms))) WHERE command_name='work.ensure' AND status='running';
UPDATE meta SET value='86' WHERE key='state_format_version';
