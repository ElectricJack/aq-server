-- Typed History attachments on supervisor replies: proposed task drafts and
-- snapshots of the session's confirmed task/graph creation receipts. Retained
-- with the immutable reply row; older replies carry an empty array.
ALTER TABLE central_supervisor_replies ADD COLUMN attachments JSONB NOT NULL DEFAULT '[]'::jsonb
 CHECK(jsonb_typeof(attachments)='array' AND jsonb_array_length(attachments)<=4
       AND octet_length(attachments::text)<=131072);
UPDATE meta SET value='63' WHERE key='state_format_version';
