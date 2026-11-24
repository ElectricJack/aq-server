-- A pull request branch the host refreshes (GitHub update-branch): one that
-- is only behind main with green checks gets main merged in without an agent
-- attempt. A row is first the task's place in its repository's queue
-- (`queued_at_ms`, `seen_at_ms` its last look), then its refresh:
-- `from_head` is the head it was asked from (the result head, or the merge
-- commit of an earlier refresh), `base_head` the main commit it was behind and
-- `merged_head` GitHub's merge commit once the wait verified its parents. A row
-- stands for its result head until `work.complete`, the close or the
-- publication of another head deletes it. Additive: an older server ignores it.
CREATE TABLE delivery_branch_refreshes (
    task_id UUID PRIMARY KEY,
    project_id UUID NOT NULL,
    repository_id UUID NOT NULL,
    result_head TEXT NOT NULL CHECK(result_head ~ '^[0-9a-f]{40}$'),
    from_head TEXT NOT NULL CHECK(from_head ~ '^[0-9a-f]{40}$'),
    base_head TEXT CHECK(base_head ~ '^[0-9a-f]{40}$'),
    merged_head TEXT CHECK(merged_head ~ '^[0-9a-f]{40}$'),
    refreshes INTEGER NOT NULL CHECK(refreshes BETWEEN 0 AND 1000),
    queued_at_ms BIGINT NOT NULL CHECK(queued_at_ms > 0),
    seen_at_ms BIGINT NOT NULL CHECK(seen_at_ms > 0),
    requested_at_ms BIGINT CHECK(requested_at_ms > 0),
    CHECK((requested_at_ms IS NULL) = (base_head IS NULL))
);
CREATE INDEX delivery_branch_refreshes_queue ON delivery_branch_refreshes(repository_id,queued_at_ms);
UPDATE meta SET value='176' WHERE key='state_format_version';
