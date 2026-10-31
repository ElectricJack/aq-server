CREATE TABLE aq_playbooks.namespaces (
    namespace text PRIMARY KEY CHECK (length(namespace) BETWEEN 1 AND 512),
    revision bigint NOT NULL DEFAULT 0 CHECK (revision >= 0),
    snapshot jsonb NOT NULL CHECK (snapshot->'artifacts' = '{}'::jsonb),
    owner text,
    incarnation bigint NOT NULL DEFAULT 0 CHECK (incarnation >= 0),
    owner_backend_pid integer,
    acquired_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK ((owner IS NULL) = (owner_backend_pid IS NULL))
);

-- Canonical artifact bytes have one address across all engine namespaces.
-- Namespace snapshots contain no duplicate artifact blobs.
CREATE TABLE aq_playbooks.artifacts (
    digest text PRIMARY KEY,
    canonical_bytes bytea NOT NULL,
    installed_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE aq_playbooks.namespace_artifacts (
    namespace text NOT NULL REFERENCES aq_playbooks.namespaces(namespace),
    digest text NOT NULL REFERENCES aq_playbooks.artifacts(digest),
    PRIMARY KEY (namespace, digest)
);
CREATE FUNCTION aq_playbooks.reject_artifact_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'aq-playbooks artifacts are immutable';
END;
$$;
CREATE TRIGGER artifacts_are_immutable BEFORE UPDATE OR DELETE
ON aq_playbooks.artifacts FOR EACH ROW
EXECUTE FUNCTION aq_playbooks.reject_artifact_mutation();
