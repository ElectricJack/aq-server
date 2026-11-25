-- The project supervisor's role text as a Settings document, read at the start
-- of each supervisor turn. NULL (never saved) means this build's compiled text.
ALTER TABLE role_settings
    ADD COLUMN supervisor_markdown TEXT,
    ADD COLUMN supervisor_revision BIGINT NOT NULL DEFAULT 0;

-- A hold no longer ends in a silent owner-attention state: after its first
-- notice it appears in every project review for as long as it lasts.
ALTER TABLE supervisor_hold_followups DROP COLUMN owner_attention;

-- An override recorded for the claiming attempt since the hold began ends it
-- (aq_delivery::git_grant::held_gate): the task now waits on what the override
-- named, so the sweep sends no notice or review for it and prunes its follow-up.
ALTER VIEW supervisor_hold_waits RENAME TO supervisor_hold_waits_recorded;
CREATE VIEW supervisor_hold_waits AS
SELECT w.* FROM supervisor_hold_waits_recorded w
WHERE NOT EXISTS (SELECT 1 FROM task_limit_overrides v WHERE v.task_id=w.task_id
  AND v.attempt_id=w.attempt_id AND v.recorded_at_ms>=w.since_ms);

UPDATE meta SET value='174' WHERE key='state_format_version';
