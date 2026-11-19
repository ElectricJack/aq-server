-- A person watching (and able to type into) a queued attempt's live terminal.
-- Written by the live terminal stream while a viewer is attached; the stall
-- ladder reads a fresh row as human control of the attempt's pane.
CREATE TABLE attempt_terminal_viewers (
    attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
    principal UUID NOT NULL,
    seen_at_ms BIGINT NOT NULL
);
UPDATE meta SET value='148' WHERE key='state_format_version';
