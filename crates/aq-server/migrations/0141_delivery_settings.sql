-- Delivery settings (spec 0016 sections 4 and 8), beside the routing document
-- in role_settings. Additive: every column is nullable or defaulted, so an
-- older server keeps running on the migrated schema. NULL document and NULL
-- limits mean "never saved": no delivery policy, and the default limits.
ALTER TABLE role_settings
    ADD COLUMN delivery_markdown TEXT,
    ADD COLUMN delivery_revision BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN delivery_limits JSONB,
    ADD COLUMN delivery_limits_revision BIGINT NOT NULL DEFAULT 0;

UPDATE meta SET value='141' WHERE key='state_format_version';
