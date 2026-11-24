-- The project supervisor's role text as a Settings document, read at the start
-- of each supervisor turn. NULL (never saved) means this build's compiled text.
ALTER TABLE role_settings
    ADD COLUMN supervisor_markdown TEXT,
    ADD COLUMN supervisor_revision BIGINT NOT NULL DEFAULT 0;

-- A hold no longer ends in a silent owner-attention state: after its first
-- notice it appears in every project review for as long as it lasts.
ALTER TABLE supervisor_hold_followups DROP COLUMN owner_attention;

UPDATE meta SET value='174' WHERE key='state_format_version';
