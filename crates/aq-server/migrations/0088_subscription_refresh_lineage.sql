-- C5b: one stored subscription login per credential, versioned. A lineage is
-- keyed by the configured credential identity, its registry revision and the
-- startup fingerprint of the configured file (base, version 0); sequence is
-- the version. `refresh` entries are lease write-backs (compare-and-swap on
-- the current version); `operator` entries are operator imports, which always
-- win. Entries never hold secret bytes:
-- the bytes live in an owner-only file named by revision_id under the
-- configured material store; only digests and sizes are recorded here.
CREATE TABLE credential_material_revisions (
 revision_id uuid PRIMARY KEY,
 credential_identity text NOT NULL CHECK (length(credential_identity) BETWEEN 3 AND 1100),
 credential_revision bigint NOT NULL CHECK (credential_revision > 0),
 base_digest text NOT NULL CHECK (base_digest ~ '^[0-9a-f]{64}$'),
 sequence bigint NOT NULL CHECK (sequence > 0),
 origin text NOT NULL CHECK (origin IN ('refresh','operator')),
 lease_id uuid NULL REFERENCES admission_material_leases(lease_id),
 content_digest text NOT NULL CHECK (content_digest ~ '^[0-9a-f]{64}$'),
 content_bytes bigint NOT NULL CHECK (content_bytes BETWEEN 1 AND 65536),
 created_at_ms bigint NOT NULL CHECK (created_at_ms > 0),
 CHECK ((origin = 'refresh') = (lease_id IS NOT NULL)),
 UNIQUE (credential_identity, credential_revision, base_digest, sequence)
);
CREATE UNIQUE INDEX credential_material_revisions_lease_digest
 ON credential_material_revisions(lease_id, content_digest) WHERE lease_id IS NOT NULL;

UPDATE meta SET value='88' WHERE key='state_format_version';
