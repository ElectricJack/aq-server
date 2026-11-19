-- The owner's limit on how old a stored Codex login may be when it is leased
-- (days since its last refresh); NULL is no limit.
ALTER TABLE provider_settings ADD COLUMN codex_login_max_age_days INTEGER
    CHECK(codex_login_max_age_days IS NULL OR codex_login_max_age_days BETWEEN 1 AND 3650);
-- A hand-started session writes a refreshed login back without an attempt
-- lease; only an operator upload must have none.
ALTER TABLE credential_material_revisions DROP CONSTRAINT credential_material_revisions_check;
ALTER TABLE credential_material_revisions ADD CONSTRAINT credential_material_revisions_check
    CHECK (origin <> 'operator' OR lease_id IS NULL);
UPDATE meta SET value='153' WHERE key='state_format_version';
