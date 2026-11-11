CREATE TABLE manual_transcript_archives (
 session_id UUID PRIMARY KEY REFERENCES manual_agent_sessions(id),
 node_id UUID NOT NULL,
 project_id UUID NOT NULL,
 last_sequence BIGINT NOT NULL DEFAULT 0 CHECK(last_sequence >= 0),
 bytes BIGINT NOT NULL DEFAULT 0 CHECK(bytes BETWEEN 0 AND 67108864),
 coverage TEXT NOT NULL DEFAULT 'unknown' CHECK(coverage IN ('unknown','complete','partial')),
 reason TEXT CHECK(length(reason)<=512),
 finalized BOOLEAN NOT NULL DEFAULT FALSE,
 updated_at_ms BIGINT NOT NULL
);
CREATE TABLE manual_transcript_chunks (
 session_id UUID NOT NULL REFERENCES manual_transcript_archives(session_id),
 sequence BIGINT NOT NULL CHECK(sequence > 0),
 text TEXT NOT NULL CHECK(octet_length(text)<=32768),
 ack_state TEXT NOT NULL CHECK(ack_state IN ('recording','finalizing')),
 PRIMARY KEY(session_id,sequence)
);
CREATE INDEX manual_agent_history_order ON manual_agent_sessions(created_at_ms DESC,id DESC);
UPDATE meta SET value='113' WHERE key='state_format_version';
