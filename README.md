# aq-server

aq-server is the AQ v2 coordination server: it owns durable execution authority, coordinates workers and profiled agent sessions, and serves human and scoped agent APIs. See the [aq-platform/agent-queue umbrella repository](https://github.com/aq-platform/agent-queue) for the platform overview.

In an umbrella checkout, this repository is the `aq-server/` submodule alongside `aq-graph/`, `aq-worker/`, `aq-git/`, `aq-playbooks/`, and `aq-dashboard/`. The sibling links below resolve in that layout.

## Current implementation

The Rust workspace includes the server, the `aq` CLI (`aqv2` crate), and the host updater (`aq-updater`). The server persists its state in PostgreSQL and integrates with independently packaged worker, Graph, Git, playbook, and dashboard components.

The implementation includes graph-backed task selection, profiled execution and result handling, scoped agent graph access and messaging, human dashboard APIs, terminal access, waits, notifications, and configured playbook and review owners. Availability depends on the configured dependencies and owners. The [agent access contract](protocol/agent-access/v1/README.md) describes the scoped interface; [CLI setup](crates/aqv2/README.md) covers its client and connection modes.

The standalone task and knowledge service is [aq-graph](https://github.com/aq-platform/aq-graph). The server consumes its versioned API rather than owning its storage. See the [Graph specification](../aq-graph/docs/specs/0001-aq-graph.md).

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

## Historical planning

The [infrastructure specification](docs/specs/0001-infrastructure-foundation.md), [implementation audit](docs/research/2026-10-30-v2-implementation-audit.md), and [milestone task graph](docs/plans/0001-v2-milestones.md) record earlier plans and evidence. They are historical references, not a current feature inventory. The milestone graph's [JSON source](docs/plans/0001-v2-milestones.json) is a local planning format, not a live AQ queue.

Validate the plan with `python3 scripts/check-v2-plan.py`; use `--ready` to list tasks whose hard prerequisites are complete.

## Development

Run the stack with `scripts/dev-up`, a worker with `scripts/dev-worker`, and the container lifecycle check with `scripts/smoke`. See [docs/development.md](docs/development.md).

Rust 1.96.1 is pinned in `rust-toolchain.toml`. Build the server, CLI, and updater with:

```sh
cargo build --locked -p aq-server -p aqv2 -p aq-updater
```

For tests, install `cargo-nextest` and configure a disposable PostgreSQL test database as described in the [development guide](docs/development.md#tests). Run:

```sh
cargo nextest run --workspace --locked --no-fail-fast
cargo test --workspace --locked --doc
```

For a focused server test run, use the binary and module filters in [AGENTS.md](AGENTS.md#running-a-subset). The development stack and smoke check require Docker with Compose; the worker must be enrolled before it can connect.

## AQ v2 skills

The [skills directory](skills/) is the source for the eight AQ operator and
agent skills, including usage of the `aq` CLI, tasks, communication, knowledge,
playbooks, reviews, mini-projects, and Git workspaces. The
playbook engine's authoring skill is maintained in the `aq-playbooks` repository.

Install or update the suite from the checkout whose revision you want to use.

```sh
python3 scripts/install-aq-skills.py
python3 scripts/install-aq-skills.py --dest ~/.claude/skills
```

The default is `${CODEX_HOME:-~/.codex}/skills`. Use `--dest` for another
harness's writable skills directory. The installer backs up replaced
`SKILL.md` files beside their originals and leaves matching files and other
skills alone. Start a new session to load the updated guidance. The installer
copies skill documents. See the [CLI setup](crates/aqv2/README.md) for the
`aq` binary and connection modes.
