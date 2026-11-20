-- Scoped draft actions are journaled under the sealed attempt identity.
ALTER TABLE dashboard_operations DROP CONSTRAINT dashboard_operation_principal;
ALTER TABLE dashboard_operations ADD CONSTRAINT dashboard_operation_principal CHECK (
 (owner_id IS NOT NULL AND owner_id=principal_id AND supervisor_session_id IS NULL)
 OR (owner_id IS NULL AND supervisor_session_id IS NOT NULL AND supervisor_session_id=principal_id
     AND kind IN ('task.create','task.graph.create'))
 OR (owner_id IS NULL AND supervisor_session_id IS NULL
     AND kind IN ('task.create','task.graph.create','task.edit','task.cancel','task.dependency.add','task.dependency.remove'))
);
UPDATE meta SET value='161' WHERE key='state_format_version';
