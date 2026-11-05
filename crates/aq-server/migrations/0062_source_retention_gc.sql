-- Release intent survives socket uncertainty; original retention receipts remain
-- immutable historical evidence, not evidence of current retention.
CREATE TABLE source_retention_releases (
 transfer_id UUID PRIMARY KEY REFERENCES source_transfers(transfer_id),
 operation_id UUID NOT NULL UNIQUE,
 handle_generation BIGINT NOT NULL CHECK(handle_generation > 0),
 reason TEXT NOT NULL,
 request JSONB NOT NULL,
 response JSONB,
 created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 released_at TIMESTAMPTZ
);
UPDATE meta SET value='62' WHERE key='state_format_version';
