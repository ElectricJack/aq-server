-- Pushed results (spec 0016 section 5.1, feature delivery_playbook). One row
-- per result report that named the task branch the node pushed
-- (AQ-Pushed-Ref, AQ-Pushed-Head), written once central aq-git's ref.show had
-- the attempt's task ref at that head; or, with head NULL, a report whose push
-- found its lease stale (AQ-Push-Conflict), once ref.show showed the task ref
-- away from the grant's lease. The branch ref is the record of the
-- result: no retention handle. Settlement facts and the delivery playbook
-- read the head here. Additive: an older server never reads this table.
CREATE TABLE delivery_pushed_results (
    report_id UUID PRIMARY KEY,
    attempt_id UUID NOT NULL REFERENCES execution_attempts(attempt_id) ON DELETE CASCADE,
    task_ref TEXT NOT NULL
        CHECK (task_ref LIKE 'refs/heads/aq/%' AND length(task_ref) <= 1024),
    head TEXT CHECK (head ~ '^[0-9a-f]{40}$'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

UPDATE meta SET value='156' WHERE key='state_format_version';
