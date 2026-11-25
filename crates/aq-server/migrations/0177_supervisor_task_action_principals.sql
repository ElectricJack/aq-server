-- A supervisor session's task actions are journaled under its session identity. Since
-- 2026-11-20 the supervisor machine route registers task.edit, task.reprioritize,
-- task.role.set, task.route.set, task.route.clear and task.dependency.add/remove, but this
-- check still allowed only the two create kinds for a supervisor session, so every such
-- journal insert failed it and the action was refused with 503 before the Graph write.
-- The supervisor domain now admits exactly the kinds its registry serves.
ALTER TABLE dashboard_operations DROP CONSTRAINT dashboard_operation_principal;
ALTER TABLE dashboard_operations ADD CONSTRAINT dashboard_operation_principal CHECK (
 (owner_id IS NOT NULL AND owner_id=principal_id AND supervisor_session_id IS NULL)
 OR (owner_id IS NULL AND supervisor_session_id IS NOT NULL AND supervisor_session_id=principal_id
     AND kind IN ('task.create','task.graph.create','task.edit','task.reprioritize','task.role.set',
                  'task.route.set','task.route.clear','task.dependency.add','task.dependency.remove'))
 OR (owner_id IS NULL AND supervisor_session_id IS NULL
     AND kind IN ('task.create','task.graph.create','task.edit','task.cancel','task.dependency.add','task.dependency.remove'))
);
UPDATE meta SET value='177' WHERE key='state_format_version';
