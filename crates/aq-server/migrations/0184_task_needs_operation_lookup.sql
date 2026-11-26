-- Offers and grant issuance fence against durable environment edits by task ID.
CREATE INDEX dashboard_task_needs_subject ON dashboard_operations
    ((response#>>'{subject_refs,0,id}'))
    WHERE kind='task.needs.set' AND disposition IN ('pending','unknown','confirmed');

UPDATE meta SET value='184' WHERE key='state_format_version';
