-- Revision pins prevent a dismissal from hiding a changed notification.
-- Pin existing dismissals to their current retained source where available.
ALTER TABLE dashboard_attention_dismissals
    ADD COLUMN expected_revision TEXT NOT NULL DEFAULT '';
UPDATE dashboard_attention_dismissals d SET expected_revision=t.xmin::text
    FROM internal_tasks t WHERE d.attention_id='internal_work:' || t.id::text;
UPDATE dashboard_attention_dismissals d SET expected_revision='sha256:' || r.report_digest
    FROM profiled_result_reports r
    WHERE d.attention_id LIKE 'result:' || r.report_id::text || ':%';
ALTER TABLE dashboard_attention_dismissals
    DROP CONSTRAINT dashboard_attention_dismissals_pkey;
ALTER TABLE dashboard_attention_dismissals
    ADD PRIMARY KEY (principal_id, attention_id, expected_revision);

UPDATE meta SET value='181' WHERE key='state_format_version';
