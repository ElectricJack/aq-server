-- agile-vault-27: owner-supplied files for the global supervisor conversation.
-- A staged upload (message_id NULL) expires after 24h; a bound attachment is
-- retained exactly as long as its owner message (ON DELETE CASCADE).
CREATE TABLE global_supervisor_attachments (
 attachment_id uuid PRIMARY KEY CHECK(attachment_id <> '00000000-0000-0000-0000-000000000000'),
 conversation_id uuid NOT NULL,
 uploader_id uuid NOT NULL,
 name text NOT NULL CHECK(octet_length(name) BETWEEN 1 AND 255),
 media_type text NOT NULL CHECK(media_type IN ('text/plain','text/markdown','text/csv','application/json','application/pdf','image/png','image/jpeg','image/gif','image/webp')),
 size_bytes bigint NOT NULL CHECK(size_bytes BETWEEN 1 AND 1048576),
 digest text NOT NULL CHECK(digest ~ '^sha256:[0-9a-f]{64}$'),
 content bytea NOT NULL,
 created_at_ms bigint NOT NULL CHECK(created_at_ms > 0),
 message_id uuid REFERENCES mailbox_messages(message_id) ON DELETE CASCADE,
 ordinal smallint CHECK(ordinal BETWEEN 0 AND 3),
 CHECK(octet_length(content) = size_bytes),
 CHECK((message_id IS NULL) = (ordinal IS NULL)),
 UNIQUE(message_id, ordinal)
);
CREATE INDEX global_supervisor_attachments_staged ON global_supervisor_attachments(conversation_id, created_at_ms) WHERE message_id IS NULL;
UPDATE meta SET value='76' WHERE key='state_format_version';
