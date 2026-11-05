-- G11 X5: pinned context refs an owner attached to a global-supervisor message.
-- Rows exist only for messages with refs and follow the message's retention.
CREATE TABLE global_supervisor_message_context (
 message_id uuid PRIMARY KEY REFERENCES mailbox_messages(message_id) ON DELETE CASCADE,
 refs jsonb NOT NULL CHECK(jsonb_typeof(refs)='array' AND jsonb_array_length(refs) BETWEEN 1 AND 16)
);
-- G11 X6: one monotonic read cursor per owner principal over the merged
-- conversation order: owner message N is (N,0), supervisor reply A.R is (A,R).
CREATE TABLE global_supervisor_read_markers (
 principal_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
 conversation_id uuid NOT NULL,
 anchor bigint NOT NULL CHECK(anchor > 0),
 reply bigint NOT NULL CHECK(reply >= 0),
 updated_at_ms bigint NOT NULL CHECK(updated_at_ms > 0),
 PRIMARY KEY(principal_id, conversation_id)
);
UPDATE meta SET value='72' WHERE key='state_format_version';
