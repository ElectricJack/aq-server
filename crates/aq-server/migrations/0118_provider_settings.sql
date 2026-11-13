-- NULL preserves startup behavior until the owner first saves provider membership.
CREATE TABLE provider_settings (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK(singleton),
    revision BIGINT NOT NULL DEFAULT 0 CHECK(revision>=0),
    providers JSONB CHECK(providers IS NULL OR jsonb_typeof(providers)='array')
);
INSERT INTO provider_settings(singleton) VALUES(TRUE);
CREATE TABLE provider_settings_operations (
    actor_id UUID NOT NULL REFERENCES users(id),
    operation_id UUID NOT NULL,
    payload_digest TEXT NOT NULL,
    response JSONB NOT NULL,
    PRIMARY KEY(actor_id,operation_id)
);
UPDATE meta SET value='118' WHERE key='state_format_version';
