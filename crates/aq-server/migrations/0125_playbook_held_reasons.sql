-- Held playbook work keeps why it was held, so the owner can see it and retry
-- it (playbook authoring B1, ruling 5). A held row has status 'unknown':
--   stale     the engine refused it as a repeat of a forgotten request;
--   uncertain the engine call's outcome is not known (timeout, owner lost);
--   refused   the engine or the server refused it;
--   invalid   (outbox only) the stored event no longer matches its record.
-- Rows held before this migration have no reason (NULL).
ALTER TABLE playbook_admissions ADD COLUMN held_reason text
    CHECK (held_reason IN ('stale','uncertain','refused'));
ALTER TABLE playbook_outbox ADD COLUMN held_reason text
    CHECK (held_reason IN ('stale','uncertain','refused','invalid'));
UPDATE meta SET value='125' WHERE key='state_format_version';
