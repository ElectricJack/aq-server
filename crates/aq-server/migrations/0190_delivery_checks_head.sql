-- The pull request head `checks_state` was evaluated on, kept with the row
-- (`work.read` catalog 2.8.0 `pull_request.checks_head`). Delivery uses it to
-- tell that a result whose head is the one whose required checks already
-- failed has nothing new to wait for. Each look at the pull request replaces
-- it, so it rolls with `checks_state`. Additive and nullable (a row written
-- before this migration, or by a command that evaluated no checks, has none);
-- an older server ignores it.
SET LOCAL lock_timeout = '5s';
ALTER TABLE delivery_pull_requests
    ADD COLUMN checks_head TEXT CHECK(checks_head IS NULL OR checks_head ~ '^[0-9a-f]{40}([0-9a-f]{24})?$');
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='190' WHERE key='state_format_version';
