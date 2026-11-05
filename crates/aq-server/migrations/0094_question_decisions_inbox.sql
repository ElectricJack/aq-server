-- F5 human question inbox: keyset pages over pending questions (oldest asked
-- first) and answered ones (most recently answered first).
CREATE INDEX question_decisions_open ON question_decisions(created_at_ms,decision_id) WHERE state='pending';
CREATE INDEX question_decisions_closed ON question_decisions(answered_at_ms DESC,decision_id DESC) WHERE state='answered';
UPDATE meta SET value='94' WHERE key='state_format_version';
