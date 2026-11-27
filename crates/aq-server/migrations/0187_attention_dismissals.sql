-- A user's cleared "Needs you" entries. Clearing hides an informational
-- notification for that user only. It is not a decision on the record behind
-- the entry: the owning record, its review gate and its state are untouched, and
-- the dashboard only accepts ids of entries that carry no pending decision.
-- Ids are the overview's attention ids; a new failure has a new id and shows again.
CREATE TABLE dashboard_attention_dismissals (
    principal_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    attention_id TEXT NOT NULL CHECK (length(attention_id) BETWEEN 1 AND 256),
    dismissed_at_ms BIGINT NOT NULL CHECK (dismissed_at_ms > 0),
    PRIMARY KEY (principal_id, attention_id)
);

UPDATE meta SET value='187' WHERE key='state_format_version';
