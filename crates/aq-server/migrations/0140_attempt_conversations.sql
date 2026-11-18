CREATE TABLE attempt_conversations (
    attempt_id UUID PRIMARY KEY REFERENCES profiled_stopped_receipts(attempt_id),
    snapshot JSONB NOT NULL
);

UPDATE meta SET value='140' WHERE key='state_format_version';
