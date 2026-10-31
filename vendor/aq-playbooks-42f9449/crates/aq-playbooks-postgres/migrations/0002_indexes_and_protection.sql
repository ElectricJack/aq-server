ALTER TABLE aq_playbooks.namespaces ALTER COLUMN snapshot DROP NOT NULL;
ALTER TABLE aq_playbooks.namespaces
    ADD COLUMN encrypted_snapshot bytea,
    ADD COLUMN protection_key_id text,
    ADD COLUMN protection_nonce bytea,
    ADD COLUMN indexes_materialized boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT snapshot_envelope_complete CHECK (
        (snapshot IS NOT NULL AND encrypted_snapshot IS NULL AND protection_key_id IS NULL AND protection_nonce IS NULL)
        OR (snapshot IS NULL AND encrypted_snapshot IS NOT NULL AND protection_key_id IS NOT NULL AND protection_nonce IS NOT NULL AND octet_length(protection_nonce)=24)
    );

CREATE TABLE aq_playbooks.artifact_catalog (
    digest text PRIMARY KEY,
    installed_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
INSERT INTO aq_playbooks.artifact_catalog(digest) SELECT digest FROM aq_playbooks.artifacts;
ALTER TABLE aq_playbooks.namespace_artifacts DROP CONSTRAINT namespace_artifacts_digest_fkey;
ALTER TABLE aq_playbooks.namespace_artifacts ADD CONSTRAINT namespace_artifacts_digest_fkey
    FOREIGN KEY(digest) REFERENCES aq_playbooks.artifact_catalog(digest);
CREATE TABLE aq_playbooks.encrypted_artifacts (
    namespace text NOT NULL REFERENCES aq_playbooks.namespaces(namespace),
    digest text NOT NULL REFERENCES aq_playbooks.artifact_catalog(digest),
    key_id text NOT NULL,
    nonce bytea NOT NULL CHECK(octet_length(nonce)=24),
    ciphertext bytea NOT NULL,
    PRIMARY KEY(namespace,digest)
);

-- Only opaque engine identifiers, contract/correlation hashes, scheduling time,
-- sequence and scope metadata are indexed. Protected payloads stay in ciphertext.
CREATE TABLE aq_playbooks.runtime_index (
    namespace text NOT NULL REFERENCES aq_playbooks.namespaces(namespace),
    kind text NOT NULL,
    lookup_key text NOT NULL DEFAULT '',
    item_key text COLLATE "C" NOT NULL,
    scope text COLLATE "C" NOT NULL,
    sequence bigint NOT NULL CHECK(sequence>=0),
    at_ms bigint,
    PRIMARY KEY(namespace,kind,lookup_key,item_key)
);
CREATE INDEX runtime_scope_queue ON aq_playbooks.runtime_index(namespace,kind,scope,sequence,item_key);
CREATE INDEX runtime_due_timer ON aq_playbooks.runtime_index(namespace,kind,at_ms,item_key) WHERE at_ms IS NOT NULL;
CREATE INDEX runtime_correlated_sequence ON aq_playbooks.runtime_index(namespace,kind,lookup_key,sequence,item_key);

CREATE TABLE aq_playbooks.inspection_rows (
    namespace text NOT NULL REFERENCES aq_playbooks.namespaces(namespace),
    kind text NOT NULL,
    item_key text COLLATE "C" NOT NULL,
    run_id text NOT NULL,
    sequence bigint NOT NULL CHECK(sequence>=0),
    public_value jsonb NOT NULL,
    PRIMARY KEY(namespace,kind,item_key)
);
CREATE INDEX inspection_by_run ON aq_playbooks.inspection_rows(namespace,kind,run_id,item_key);

CREATE TABLE aq_playbooks.disposal_tombstones (
    namespace text NOT NULL REFERENCES aq_playbooks.namespaces(namespace),
    kind text NOT NULL,
    identity text NOT NULL,
    content_digest text,
    disposed_at_ms bigint NOT NULL,
    PRIMARY KEY(namespace,kind,identity)
);
CREATE INDEX disposal_expiry ON aq_playbooks.disposal_tombstones(namespace,kind,disposed_at_ms,identity);

-- Normal SQL updates/deletes remain forbidden. Owner-validated GC may delete an
-- unreferenced payload only after recording its permanent canonical-byte digest.
CREATE OR REPLACE FUNCTION aq_playbooks.reject_artifact_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' AND (
        (NOT EXISTS (SELECT 1 FROM aq_playbooks.namespace_artifacts WHERE digest=OLD.digest)
        AND EXISTS (SELECT 1 FROM aq_playbooks.disposal_tombstones WHERE kind='artifact' AND identity=OLD.digest))
        OR (EXISTS (SELECT 1 FROM aq_playbooks.encrypted_artifacts WHERE digest=OLD.digest)
        AND NOT EXISTS (SELECT 1 FROM aq_playbooks.namespace_artifacts n JOIN aq_playbooks.namespaces s ON s.namespace=n.namespace WHERE n.digest=OLD.digest AND (s.protection_key_id IS NULL OR NOT EXISTS (SELECT 1 FROM aq_playbooks.encrypted_artifacts e WHERE e.namespace=n.namespace AND e.digest=n.digest))))
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'aq-playbooks artifacts are immutable';
END;
$$;
