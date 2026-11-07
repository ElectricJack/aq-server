# worker-machine/v1

The machine load report a worker posts to aq-server (spec 0007 section 6).
`schema.json` is generated from `aq_swarm::machine::MachineReport` by the test
`worker_machine_schema_matches_protocol` in `aq-swarm` when `AQ_WRITE_PROTOCOL=1`
is set. `check.py` validates the fixtures and a few invalid cases.

## Route

`POST /v1/worker/machine` on the worker listener, with the node bearer
credential (`Authorization: Bearer aqn_...`) and a JSON `MachineReport` body.
Success is `200 {"recorded": true}`. An invalid report gets `400`; an unknown
or revoked credential gets the same errors as the heartbeat. An older server
answers 404, and the worker keeps its interval.

The worker posts every 15 s.

## Fields

- `schema_version`: 1.
- `machine.key`: 1-63 characters of `[a-z0-9-]`, not starting with `-`. Nodes
  that report the same key form one machine. Absent: the node is its own machine.
- `machine.label`: display name, 1-64 characters, no control characters.
- `machine.kind`: `local` or `cloud`.
- `observed_at_ms`: worker clock, milliseconds since the Unix epoch. The server
  uses its own receive time for freshness.
- `source`: `linux` (the machine's own load) or `windows_host` (the Windows
  host's load, read on a WSL2 worker).
- `host_probe`: `not_applicable`, `fresh`, `stale` or `missing`.
- `logical_cpus`: 1-4096.
- `memory_total_bytes`, `memory_used_bytes`: bytes, used at most total.
- `cpu_busy_bps`: CPU busy in basis points, 0-10000.
- `disk_total_bytes`, `disk_free_bytes`: both present or both absent, free at
  most total.

## Server use

A report is fresh when received within 60 s. A sample is stored only when the
node's newest sample is at least 5 s older; the latest report is always updated. The 15-minute average is the mean
of the samples received in the last 900 000 ms. Memory is stored as basis
points of used over total.

The worker pins this directory by digest.
