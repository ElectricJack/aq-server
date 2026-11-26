-- The node limits become a Settings value: the sandbox headroom a node needs
-- before it admits work, the build-cache toolchain keys it keeps per project,
-- and how long a deferred admission waits before it is owner attention.
-- Additive: NULL means never saved, the defaults of the build.
SET LOCAL lock_timeout = '5s';
ALTER TABLE role_settings
    ADD COLUMN node_limits JSONB,
    ADD COLUMN node_limits_revision BIGINT NOT NULL DEFAULT 0;

UPDATE meta SET value='186' WHERE key='state_format_version';
