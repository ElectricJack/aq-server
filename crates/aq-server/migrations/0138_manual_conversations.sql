CREATE TABLE manual_conversations (
    session_id UUID PRIMARY KEY REFERENCES manual_agent_sessions(id) ON DELETE CASCADE,
    snapshot JSONB NOT NULL
);
UPDATE meta SET value='138' WHERE key='state_format_version';
