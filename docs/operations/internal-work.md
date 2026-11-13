# Operating the internal work host

The internal work host is `aq-supervisor-host internal <CONFIG>`. It claims
queued internal tasks that use a subscription login (Codex or Claude) from the
database and runs the model CLI for each, up to two at a time, so that one long
run does not keep the next task waiting. At most one of them is a Codex run: two
Codex runs on one login could both spend the same single-use refresh token, so
while a Codex run holds the login (until its refresh is copied back) the host
claims only Claude tasks. Claude runs can share their token, which is never
refreshed. Spec:
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

## Ownership the host enforces

The host refuses to start, or fails the run, unless these hold. Inside the
container the host runs as root, so "owned by the host" means owned by root.

- **Config file** (`protected_read`, `config.rs`): absolute canonical path, not a
  symlink, a regular file with one link, owned by the host's effective uid (root),
  no group or other permission bits (mode 0600), at most 64 KiB. Otherwise the
  host exits with `protected file rejected`.
- **Login files** (`secret_file`): the same `protected_read` check (owned by the
  host's uid, no group or other bits, one link, not a symlink), at most 64 KiB,
  and the file must be mounted at an absolute canonical path.
- **Native executables** (`subscription.rs`): owned by uid 0, not group or world
  writable, with an execute bit, and matching `native_digest`.
- **`homes` and `scratch`** (`private_parent`, `internal.rs`): canonical
  directories owned by uid 0 and not group or world writable (no `0o022` bits).

Files made by the developer (for example the one `scripts/dev-stack.py` writes,
owned by uid 1000 and holding the database URL) fail these checks. Make them
root-owned in one of two ways:

1. Copy the file to the directory you mount at `/config` (or the login
   directories) and run `sudo chown root:root <file> && sudo chmod 0600 <file>`.
   Every file the host reads must be done this way, and the directory containing
   it must not let another user replace the file.
2. Place the file in a volume or container with root ownership, the way the local
   supervisor manager places supervisor host config. The repository documents the
   requirement, not a procedure: [the supervisor host README](../../crates/aq-supervisor-host/README.md)
   says to mount `secret_file` and the native executable root-owned, and
   [supervisors](supervisors.md) covers the manager. I found no documented
   tar-copy procedure to point to, so use way 1 unless your manager provides one.

## Config file

`/config/internal.json` must be an absolute, canonical, owner-only (0600)
regular file of at most 64 KiB, owned by root in the container (see above). Unknown fields are rejected.

| Field | Meaning |
| --- | --- |
| `database_url` | Database the host claims from (it holds the password, hence 0600 and root ownership). |
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
- A host that stopped mid-run can leave a run home holding a Codex login the run
  refreshed. At start the host copies such a login back when it is the same
  account and its `last_refresh` is later than the login file's, then removes the
  run directories (only entries named by a UUID). A home whose newer login could
  not be copied back is kept and the host prints
  `internal work: kept a run home whose newer login was not copied back`.
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
  `internal work: claim failed` (a poll failed; it retries after `poll_ms`),
  `internal work: run failed` (recording a run's result failed),
  `internal work: a native executable was refused` (at start, a native whose
  SHA-256 does not match the config; its runs fail) and
  `internal work: login refresh was not copied back`. Secrets never appear in logs.
- A container that stays up with no `claim failed` or `run failed` lines and no
  failed rows is polling. A bad config makes the process exit at start with the
  reason on stderr.
- Each native is hashed when the host starts and again only when its file's
  device, inode, size, mtime or ctime change.
- A config change, or a config file the host can no longer accept, takes effect
  only after the runs in progress have ended (each within its task deadline, at
  most 900 s): the host stops claiming, lets them finish and copy their logins
  back, then exits (with the error, if the file was refused) to be restarted.

## Time limits

- Each candidate's budget is the time left to the task deadline when it is
  claimed, shared evenly among the candidates not yet tried, at least 30 s and
  never more than is left. The host and the server stop a candidate at most 1 s
  before its budget ends and record `deadline_exceeded` with "`<harness>` did not
  answer within N s".
- The server's sweep fails a `running` candidate 5 s after its budget ends, with
  "runner did not report". That means the runner died or lost the database.
- The sweep compares the server's clock with the claim time the host wrote, so
  the host's and the server's clocks must agree to well under those 5 s (NTP).
- The server runs up to four API tasks at once and sweeps on its own interval,
  so a slow provider call holds up neither other tasks nor the sweep.

## Capacity

Internal work is outside swarm capacity. It adds no `swarm_supervisor_admissions`
rows and the swarm availability switch does not stop it.

## Local stack

`scripts/dev-stack.py prepare` writes `<state>/internal/internal.json` when
`AQ_DEV_INTERNAL_DATABASE_URL` and, per harness, `AQ_DEV_INTERNAL_CODEX_NATIVE` and
`AQ_DEV_INTERNAL_CODEX_LOGIN` (or the `CLAUDE` pair) are set; it hashes the native
file itself and uses the container paths above. Without them nothing is written.
Optional: `AQ_DEV_INTERNAL_POLL_MS`. The generated file is owned by you and holds the database URL: do not commit or share it, and make a root-owned copy before mounting it (see above). It prints `AQ_INTERNAL_CONFIG_DIR` for use with
`compose.workers.yaml`; the other `AQ_INTERNAL_*` variables are yours to set.

## Supervisor access

A supervisor can change Intelligence settings and see and retry internal runs
(spec 0012 §8). It calls the owner handlers through `/v1/supervisor/admin` on the
machine listener, with its bearer and no CSRF header. The credential needs the
`settings` scope. Only paths under these three prefixes are reachable; every other
path is 404:

- `/api/settings/intelligence` (profiles, and the routing policy at `/structure`)
- `/api/settings/internal-work` (the internal work default and overrides)
- `/api/internal-work/` (runs, a run, and retry)

New supervisor credentials carry six scopes, including `settings`. A credential
issued before this change keeps its five scopes and gets 403 on
`/v1/supervisor/admin` until it is reissued. A supervisor gets a new credential
when its next session starts. A save is recorded under the user id of the owner
who approved the supervisor.
