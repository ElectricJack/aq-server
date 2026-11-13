# Operating the internal work host

The internal work host is `aq-supervisor-host internal <CONFIG>`. It claims
queued internal tasks that use a subscription login (Codex or Claude) from the
database and runs the model CLI for each, one at a time. Spec:
[0012 internal work](../specs/0012-internal-work.md). Build notes:
[implementation](../implementation/0012-internal-work.md).

## What runs where

- The server (`[internal_work]` in the vault `config.toml`) queues tasks, sweeps
  expired ones and, when `enabled` is true (the default), claims tasks for the
  configured API endpoints. `enabled = false` stops only the server claiming API
  work; sweeping always runs. The section rejects unknown keys. Keys: `api`
  (list of `provider`, `endpoint`, `secret_file`), `poll_ms` (default 200),
  `enabled`.
- The host runs the login tasks. It is a separate process, in the
  `internal-work` service of `compose.workers.yaml` (same image as the
  supervisor host, started with `--profile internal-work`).

## Requirements

- Run as root. The host drops to uid/gid 10001 for each model CLI run.
- Its own PID namespace (Compose's default; never `pid: host`) and no Docker
  socket. Network: the database and provider egress only.
- Nothing untrusted may run as uid 10001 in the container. The model CLI's
  environment and its staged login are readable by that uid.
- The image holds neither CLI. Mount both native executables read-only and put
  each one's SHA-256 in the config.

## Config file

`/config/internal.json` must be an absolute, canonical, owner-only (0600)
regular file of at most 64 KiB. Unknown fields are rejected.

| Field | Meaning |
| --- | --- |
| `database_url` | Database the host claims from (it holds the password, hence 0600). |
| `homes` | Parent of the per-run native homes. Canonical, owned by the executable owner, not group or world writable. |
| `scratch` | Parent of the per-run empty working directories. Same rules. |
| `poll_ms` | 1 to 60000. Sleep between polls when nothing was claimed. |
| `codex`, `claude` | Optional. Each: `native` (absolute path), `native_digest` (64 hex characters, SHA-256 of the executable), `secret_file` (the login file). |

A host claims tasks only for the harnesses it configures. Login files must not be
under `homes` or `scratch`. The host returns, so a supervisor restarts it, when
the config file is removed or its content changes.

## What to mount (`compose.workers.yaml`)

Set `AQ_SUPERVISOR_HOST_IMAGE` (shared with the supervisor host) and:

| Variable | Mounted at | Mode |
| --- | --- | --- |
| `AQ_INTERNAL_CONFIG_DIR` (holds `internal.json`) | `/config` | read-only |
| `AQ_INTERNAL_CODEX_BIN` | `/opt/codex/codex` | read-only |
| `AQ_INTERNAL_CLAUDE_BIN` | `/opt/claude/claude` | read-only |
| `AQ_INTERNAL_CODEX_LOGIN_DIR` (this host's own `auth.json`) | `/logins/codex` | writable; add `read_only: true` to forbid write-back |
| `AQ_INTERNAL_CLAUDE_LOGIN_DIR` (setup token file) | `/logins/claude` | read-only |

`homes` and `scratch` are private named volumes at `/internal/homes` and
`/internal/scratch`. Example config paths: `native` `/opt/codex/codex`,
`secret_file` `/logins/codex/auth.json`.

Start: `docker compose -f compose.yaml -f compose.workers.yaml --profile internal-work up -d internal-work`.

## Logins

- Give each internal work host its own copy of each login file. Sharing one file
  between hosts relies on compare-and-swap and loses a refresh when two hosts
  refresh at once.
- Codex: the file is copied in and back under a read-only `flock` on the login
  file's directory. The directory must be writable only if a refreshed login is to
  be kept; a refresh is written by temp-file-and-rename, and only if the file still
  holds the bytes the run copied in. If a refresh could not be copied back the host
  prints `internal work: login refresh was not copied back`.
- Claude: a one-line setup token file. It is passed to the CLI only as an
  environment variable and never written to disk.
- The Codex CLI cannot have its shell tool removed. The runner uses a read-only
  sandbox in an empty directory.
- Claude tools are disabled with `--tools ""`. That was read from the help of
  Claude Code 2.1.323 and 2.1.319. Before deploying, confirm it appears in
  `--help` of the exact binary mounted into the container.
- Usage-limit and login failures are recognised by message wording that has not
  yet been checked against a live failure. An unrecognised message still moves the
  task to the next provider, recorded as `provider_error`.

## Telling the host is working

- A task being run has a `running` row in `internal_tasks` whose `runner` column
  is `internal-host:<hostname>`:
  `SELECT id, status, runner FROM internal_tasks WHERE status = 'running';`
- The host prints no line when it claims a task. Its only log lines are
  `internal work: tick failed` (a poll failed; it retries after `poll_ms`) and
  `internal work: login refresh was not copied back`. Secrets never appear in logs.
- A container that stays up with no `tick failed` lines and no failed rows is
  polling. A bad config makes the process exit at start with the reason on stderr.

## Capacity

Internal work is outside swarm capacity. It adds no `swarm_supervisor_admissions`
rows and the swarm availability switch does not stop it.

## Local stack

`scripts/dev-stack.py prepare` writes `<state>/internal/internal.json` when
`AQ_DEV_INTERNAL_DATABASE_URL` and, per harness, `AQ_DEV_INTERNAL_CODEX_NATIVE` and
`AQ_DEV_INTERNAL_CODEX_LOGIN` (or the `CLAUDE` pair) are set; it hashes the native
file itself and uses the container paths above. Without them nothing is written.
Optional: `AQ_DEV_INTERNAL_POLL_MS`. It prints `AQ_INTERNAL_CONFIG_DIR` for use with
`compose.workers.yaml`; the other `AQ_INTERNAL_*` variables are yours to set.
