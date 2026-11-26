# PostgreSQL terminal test environment

The non-default `postgres17-terminal-test` recipe provides `postgres` and
`terminal-test`. A task that needs these tools can request `terminal-test`
alongside its PostgreSQL service. The existing PostgreSQL-only recipe stays
available for tasks that only need the database.

Use the normal AQ project environment builder with
[`.aq/environments/postgres17-terminal-test.md`](../../.aq/environments/postgres17-terminal-test.md).
The builder supplies the approved Debian Bookworm OS and the separately pinned
dual-harness runtime. The recipe installs the same PostgreSQL 17.12 package
versions as the PostgreSQL-only recipe. It verifies the Node 24.11.0 and pnpm
10.34.5 release archive SHA-256 values and installs their commands on the
worker's fixed shell path, `/usr/local/bin:/usr/bin:/bin`. The checks run
as the image's agent user and verify tool resolution and PostgreSQL cluster initialization.

On a checkout carrying the dashboard release scripts, run
`PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-dashboard-candidate.py`.
These checks use the checkout and task-local scratch space. Use the existing
locked server and dashboard CI workflows for Rust and dependency-backed
dashboard checks, and the isolated host pilot for a real browser and worker
terminal connection. The recipe includes no project dependency stores or
browser binaries.
