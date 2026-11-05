-- Plaintext binding of each new admission, and lookup by accepted run, so a
-- playbook run read can show its binding and activation revision. Admissions
-- prepared before this migration keep a null binding.
ALTER TABLE playbook_admissions ADD COLUMN binding text
 CHECK (binding IS NULL OR octet_length(binding) BETWEEN 1 AND 512);
CREATE INDEX playbook_admissions_run ON playbook_admissions(namespace_id,generation,(receipt->>'run_id'))
 WHERE receipt IS NOT NULL;
UPDATE meta SET value='90' WHERE key='state_format_version';
