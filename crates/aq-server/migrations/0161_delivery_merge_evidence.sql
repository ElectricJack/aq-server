-- Spec 0019: GitHub merge evidence survives branch cleanup and host receipt
-- retention. The key is the existing host operation id.
CREATE TABLE delivery_merge_evidence (
    operation_id UUID PRIMARY KEY,
    project_id UUID NOT NULL,
    task_id UUID NOT NULL,
    attempt_id UUID,
    result_id UUID,
    pull_request BIGINT NOT NULL CHECK(pull_request BETWEEN 1 AND 1000000000),
    target JSONB NOT NULL,
    target_ref TEXT NOT NULL,
    expected_head_sha TEXT NOT NULL CHECK(expected_head_sha ~ '^[0-9a-f]{40}([0-9a-f]{24})?$'),
    merge_sha TEXT NOT NULL CHECK(merge_sha ~ '^[0-9a-f]{40}([0-9a-f]{24})?$' AND merge_sha !~ '^0+$'),
    observed_at_ms BIGINT NOT NULL CHECK(observed_at_ms > 0),
    CHECK((attempt_id IS NULL) = (result_id IS NULL))
);
UPDATE meta SET value='161' WHERE key='state_format_version';
