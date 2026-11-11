-- Manual provider availability is reported by the original enrolled worker.
ALTER TABLE manual_worker_reports ADD COLUMN providers JSONB NOT NULL DEFAULT '["anthropic"]'::jsonb CHECK(jsonb_typeof(providers)='array');
UPDATE meta SET value='108' WHERE key='state_format_version';
