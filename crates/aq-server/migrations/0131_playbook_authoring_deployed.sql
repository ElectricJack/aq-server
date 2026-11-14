-- Playbook authoring: whether the engine has acknowledged the active run's
-- artifact on its binding. A save records the run active first and activates
-- in the engine after; until the engine has it, the server keeps trying.
ALTER TABLE playbook_authoring_runs ADD COLUMN deployed BOOLEAN NOT NULL DEFAULT TRUE;
UPDATE meta SET value='131' WHERE key='state_format_version';
