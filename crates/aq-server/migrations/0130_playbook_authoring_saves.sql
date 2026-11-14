-- Playbook authoring, save and take effect (spec 0013 §4.3-§4.5).
-- A saved run keeps the document and the cases it was saved with, so the next
-- run's agent is shown the previous playbook in the owner's terms, and when
-- its artifact took effect, so the view can say since when. A deleted
-- registered source stays as a tombstone: its run history refers to it.
ALTER TABLE playbook_authoring_runs
    ADD COLUMN markdown TEXT,
    ADD COLUMN case_files JSONB,
    ADD COLUMN activated_at_ms BIGINT;
ALTER TABLE playbook_sources ADD COLUMN deleted_at_ms BIGINT;
UPDATE meta SET value='130' WHERE key='state_format_version';
