# aq-server

The new Agent Queue foundation, developed as a fresh start alongside the existing installation. V1 migration is not a launch requirement. Any optional owner-specific data transfer belongs in an external one-off script outside aq-graph.

Start with [Phase 1: Infrastructure foundation](docs/specs/0001-infrastructure-foundation.md).

The initial deployment targets a Bluehost self-managed VPS with Docker Compose and Cloudflare DNS, Tunnel, and Access. Phase one covers hosting, login, worker connectivity, development, deployment, self-update, and recovery.

The foundation has passed local qualification. This repository now owns durable execution authority and consumes an independently packaged Rust worker. The first execution slice runs a deterministic fixture agent with replay, cancellation and recovery checks; production graph scheduling, real agent harnesses and policy workflows remain later milestones. See the [implementation evidence](docs/implementation/execution-authority.md) and current [milestone status](docs/plans/0001-v2-milestones.md).

The worker recovery implementation uses a Linux guardian with independent expiry and conservative recovery when process ownership is uncertain. See [recovery authority](docs/implementation/recovery-authority.md). The separately tested [scoped agent graph contract](protocol/agent-access/v1/README.md) defines the next integration boundary; its production gateway and client remain to be implemented.

The standalone task and knowledge service is **aq-graph**, maintained in its own repository. Its [draft specification](../aq-graph/docs/specs/0001-aq-graph.md) covers the Rust service and CLI, PostgreSQL storage, selective history, and concurrent access. This relative link assumes the repositories are checked out side by side. Agent Q will consume its versioned API; this component does not expand the infrastructure phase.

## Architecture and component specifications

- [Ecosystem, repository ownership and product releases](docs/architecture/0001-ecosystem-and-releases.md)
- [Shared agent execution and recovery](docs/architecture/0002-execution-and-recovery.md)
- [Agent profiles, intelligence, mandatory tmux hosting and remote terminals](docs/architecture/0003-agent-profiles-harnesses-and-terminals.md)
- [Session observability, stall recovery, notifications and dependency-aware hibernation](docs/architecture/0004-session-operations-waits-and-notifications.md)
- [Daemon execution coordination and remote agent graph access](docs/specs/0002-execution-and-agent-access.md)
- [Worker framework](../aq-worker/docs/specs/0001-worker-framework.md) and [worker execution/agent access](../aq-worker/docs/specs/0002-execution-and-agent-access.md)
- [Git interface](../aq-git/docs/specs/0001-aq-git.md) and [playbook engine](../aq-playbooks/docs/specs/0001-aq-playbooks.md)
- [Dashboard](../aq-dashboard/docs/specs/0001-dashboard.md)

Component implementations remain in independently tested repositories. AQ assembles pinned libraries, executables, images and the dashboard bundle into a tested product release; ordinary builds do not require sibling source checkouts or submodules.

## Implementation planning

The [implementation audit](docs/research/2026-10-30-v2-implementation-audit.md) distinguishes existing code and recorded evidence from missing integration work. The [milestone task graph](docs/plans/0001-v2-milestones.md) defines repository ownership, prerequisites and acceptance for the next implementation slices. Its [JSON source](docs/plans/0001-v2-milestones.json) is a local planning format, not a live AQ queue.

Validate the plan with `python3 scripts/check-v2-plan.py`; use `--ready` to list tasks whose hard prerequisites are complete.

## Development

**To run a complete local AQ (server, dashboard, routing and a worker that runs real tasks), follow [docs/getting-started.md](docs/getting-started.md).**

For the bare server stack only: run it with `scripts/dev-up`, a worker with `scripts/dev-worker`, and the container lifecycle check with `scripts/smoke`. See [docs/development.md](docs/development.md).
