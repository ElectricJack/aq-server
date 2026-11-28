-- Workers take every repository from the server (aq-worker PR #135), so the
-- hand-started-session poll no longer reports a repository list. A session
-- starts for any bound project the owner allows on a live worker that reports
-- the provider; its repository comes from the project binding. A server older
-- than this format writes the dropped column and cannot use this database.
SET LOCAL lock_timeout = '5s';
ALTER TABLE manual_worker_reports DROP COLUMN repositories;
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='191' WHERE key='state_format_version';
