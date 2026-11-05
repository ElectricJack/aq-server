-- C4: activation-time pin of each catalog environment to one allowlisted
-- template image digest. Only a later activation of the same catalog replaces
-- them; running attempts keep the digest frozen in their launch manifest.
CREATE TABLE profile_catalog_environment_templates (
 catalog_digest TEXT NOT NULL REFERENCES profile_catalog_artifacts(digest),
 environment TEXT NOT NULL CHECK (length(environment) BETWEEN 1 AND 512),
 template_digest TEXT NOT NULL CHECK (template_digest ~ '^[0-9a-f]{64}$'),
 PRIMARY KEY (catalog_digest, environment)
);
UPDATE meta SET value='55' WHERE key='state_format_version';
