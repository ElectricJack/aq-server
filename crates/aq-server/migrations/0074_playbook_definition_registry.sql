-- G11 (X30/X32): AQ-side registry of installed playbook definitions for the
-- dashboard Playbooks view. The engine has no definition version, so AQ
-- assigns a per-name ordinal the first time an artifact is installed in a
-- namespace generation. `graph` is the public step graph only: step ids,
-- operation kinds, command names and edges; never expressions or values.
-- Existing rows and tables are unchanged.
CREATE TABLE playbook_definitions (
 namespace_id uuid NOT NULL,
 generation bigint NOT NULL,
 artifact text NOT NULL CHECK (octet_length(artifact) BETWEEN 1 AND 512),
 name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 512),
 version bigint NOT NULL CHECK (version > 0),
 graph jsonb CHECK (graph IS NULL OR octet_length(graph::text) <= 65536),
 registered_at_ms bigint NOT NULL,
 PRIMARY KEY(namespace_id,generation,artifact),
 UNIQUE(namespace_id,generation,name,version),
 FOREIGN KEY(namespace_id,generation) REFERENCES playbook_namespaces(namespace_id,generation)
);
UPDATE meta SET value='74' WHERE key='state_format_version';
