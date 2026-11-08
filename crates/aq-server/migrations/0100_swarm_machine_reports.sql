-- Spec 0007 phase 2: latest machine report per node and a 15-minute load
-- window for the swarm read. received_at_ms is the server clock.
CREATE TABLE node_machine_reports (
 node_id uuid PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
 report jsonb NOT NULL CHECK (octet_length(report::text) <= 2048),
 received_at_ms bigint NOT NULL CHECK (received_at_ms > 0)
);
CREATE TABLE node_machine_samples (
 node_id uuid NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
 received_at_ms bigint NOT NULL,
 cpu_busy_bps smallint NOT NULL CHECK (cpu_busy_bps BETWEEN 0 AND 10000),
 memory_used_bps smallint NOT NULL CHECK (memory_used_bps BETWEEN 0 AND 10000),
 PRIMARY KEY (node_id, received_at_ms)
);
UPDATE meta SET value='100' WHERE key='state_format_version';
