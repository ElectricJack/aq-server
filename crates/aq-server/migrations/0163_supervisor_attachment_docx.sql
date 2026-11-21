-- Owner supervisor attachments accept DOCX (WordprocessingML) files. The
-- size, name, digest and binding constraints of migration 76 are unchanged.
ALTER TABLE global_supervisor_attachments
    DROP CONSTRAINT global_supervisor_attachments_media_type_check,
    ADD CONSTRAINT global_supervisor_attachments_media_type_check CHECK(media_type IN (
        'text/plain','text/markdown','text/csv','application/json','application/pdf',
        'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
        'image/png','image/jpeg','image/gif','image/webp'));
UPDATE meta SET value='163' WHERE key='state_format_version';
