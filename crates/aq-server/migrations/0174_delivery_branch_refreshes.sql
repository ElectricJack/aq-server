-- A branch refresh the host asked GitHub for (update-branch): a pull request
-- that was only behind main with green checks gets main merged in without an
-- agent attempt. `from_head` is the head it was asked from (the result head,
-- or the merge commit of an earlier refresh), `base_head` the main commit it
-- was behind and `merged_head` GitHub's merge commit once the wait verified its
-- parents. A row stands for its result head until the merge, the close or the
-- next publication of the task deletes it; a young one holds its repository's
-- one refresh slot. Additive: an older server ignores the table.
CREATE TABLE delivery_branch_refreshes (
    task_id UUID PRIMARY KEY,
    project_id UUID NOT NULL,
    repository_id UUID NOT NULL,
    result_head TEXT NOT NULL CHECK(result_head ~ '^[0-9a-f]{40}$'),
    from_head TEXT NOT NULL CHECK(from_head ~ '^[0-9a-f]{40}$'),
    base_head TEXT NOT NULL CHECK(base_head ~ '^[0-9a-f]{40}$'),
    merged_head TEXT CHECK(merged_head ~ '^[0-9a-f]{40}$'),
    refreshes INTEGER NOT NULL CHECK(refreshes BETWEEN 1 AND 1000),
    requested_at_ms BIGINT NOT NULL CHECK(requested_at_ms > 0)
);
CREATE INDEX delivery_branch_refreshes_repository ON delivery_branch_refreshes(repository_id,requested_at_ms);
UPDATE meta SET value='174' WHERE key='state_format_version';
