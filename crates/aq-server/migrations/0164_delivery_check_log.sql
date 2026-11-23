-- Spec 0021 section 5.3: the redacted tail of a failed required check's job
-- log (`check_log_bytes`), kept with the pull request row it was read for and
-- the head it was read at. Each look at the pull request replaces it, so it
-- rolls with the row. Additive and nullable; an older server ignores it.
SET LOCAL lock_timeout = '5s';
ALTER TABLE delivery_pull_requests
    ADD COLUMN checks_log_head TEXT CHECK(checks_log_head IS NULL OR checks_log_head ~ '^[0-9a-f]{40}([0-9a-f]{24})?$'),
    ADD COLUMN checks_log_tail TEXT CHECK(checks_log_tail IS NULL OR octet_length(checks_log_tail) BETWEEN 1 AND 1048576),
    ADD CHECK((checks_log_head IS NULL) = (checks_log_tail IS NULL));
SET LOCAL lock_timeout TO DEFAULT;
UPDATE meta SET value='164' WHERE key='state_format_version';
