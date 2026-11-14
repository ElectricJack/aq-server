# Operating the internal work host

2026-11-13: aligned with the rolling-records principle
([architecture 0006](../architecture/0006-records-are-rolling.md));
implementation pending.

The internal work host is `aq-supervisor-host internal <CONFIG>`. It claims
queued internal tasks that use a subscription login (Codex or Claude) from the
database and runs the model CLI for each, up to two at a time, so that one long
run does not keep the next task waiting. Spec:
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

- Run as root. The host drops to uid/gid 10001 for each model CLI run, and to
  10002 for agent work (see "Agent work").
- Its own PID namespace (Compose's default; never `pid: host`) and no Docker
  socket. Network: the database and provider egress only.
- Nothing untrusted may run as uid 10001 or 10002 in the container. The model
  CLI's environment and its staged login are readable by the uid it runs as.
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
  not be copied back, or that holds a login while Codex is not configured, is kept
  and the host prints
  `internal work: kept a run home holding a login not copied back`.
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
  `internal work: login refresh was not copied back` and
  `internal work: recording a run timed out` (a result write took over 30 s; the
  sweep moves the task on). Secrets never appear in logs.
- A container that stays up with no `claim failed` or `run failed` lines and no
  failed rows is polling. A bad config makes the process exit at start with the
  reason on stderr.
- Each native is hashed when the host starts and again only when its file's
  device, inode, size, mtime or ctime change.
- A config change, or a config file the host can no longer accept, takes effect
  only after the runs in progress have ended (each within its task deadline, at
  most 900 s, or 1 800 s for agent work): the host stops claiming, lets them
  finish and copy their logins back, then exits (with the error, if the file
  was refused) to be restarted.

## One Codex run per login

- Each host runs at most one Codex run at a time on its Codex login: two Codex
  runs on one login could both spend the same single-use refresh token. A Codex
  run holds the login until it has ended and its refresh is copied back,
  including after a timeout or a crash of the run. Claude runs share their token,
  which is never refreshed, and can use both slots.
- While the login is busy, a queued task whose current candidate is Codex either
  waits or is passed over. It is passed over when it has a later candidate and is
  blocking or has less than 60 s left: its Codex attempt is recorded with reason
  `login_busy` and detail `codex login busy`, and the next candidate runs at once.
  This says nothing about the provider or the login's health; it is not a
  usage-limit or provider failure, and it never fails a task by itself or
  messages a supervisor. Other tasks, and a task whose only remaining candidate is
  Codex, wait for the login.
- A Codex candidate that falls back to another Codex candidate on the same host
  rarely has time to run: the first candidate takes all but a second of a
  deadline under 60 s, and the second waits for the first's copy-back. Put a
  candidate of another harness or an API candidate after a Codex one.

## Agent work

A request with `agent` work (spec 0013 §4.2) runs the harness with tools in a
workspace instead of as one prompt. Its task row has `workspace_kind = 'agent'`.

- **Its own uid.** Agent work runs as uid/gid **10002**; answer runs stay
  10001. So an agent cannot read another run's process environment
  (`/proc/<pid>/environ`), home or workspace, and no other run can read the
  agent's. When an agent run ends (answer, failure, timeout, a dropped or
  panicked run) the host kills every process of uid 10002, a process that left
  the session included, and removes what 10002 left at the top of `/tmp`. The
  host does the same at start. Nothing else may run as 10002.
- **Workspace.** A fresh directory under `scratch` per candidate, built while
  it is still root's and then handed to 10002 without following links. It holds
  only the request's files (relative paths, segments of at most 255 bytes,
  paths of at most 4096, at most 256 files and 1 MiB) and the agent's own
  temporary directory `.tmp` (its `TMPDIR`). It is removed when the candidate
  ends, however it ends.
- **Home.** `homes/<run>` stays root's (0711). It holds the agent's own
  `home/` (its `HOME`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, owned by 10002), and,
  root-owned and not writable by the agent, Claude's `settings.json` (0644) and
  `bin/`, the shims. A leftover Codex login of an agent run is
  `homes/<run>/home/auth.json`; startup recovery offers it back like any other.
- **The engine CLI and its shim.** The agent's `PATH` starts with
  `homes/<run>/bin`, where `aq-playbooks` is a shim: it runs the real binary,
  `/usr/local/libexec/aq-playbooks`, only when its working directory is the
  workspace, with an environment of `PATH`, `HOME`, `TMPDIR` and `LANG` only, so
  a login token in the harness's environment never reaches it. The real binary
  must not be on `PATH`. Its own contract (the engine CLI's): every path
  argument is relative to the workspace, without `..` or a link.
- **Answer.** The file `out/outcome.json` in the workspace, opened without
  following links, a regular file with one link and at most the request's
  `max_output_bytes`, then checked against the request's answer schema like any
  answer. No outcome, a link, a file that is too large or malformed fails the
  candidate as `invalid_output` ("the agent ended without saving, asking or
  giving up" when there is none).
- **Claude tools:** `--restricted --permission-mode dontAsk --tools
  Bash,Read,Write,Edit,Glob,Grep --allowedTools "Bash(aq-playbooks:*)" Read
  Write Edit Glob Grep --disallowedTools WebFetch WebSearch --max-turns N`, with
  `--output-format stream-json --verbose`. `--max-turns` is accepted but not
  listed in `--help` of Claude Code 2.1.323. Confirm the flags against the
  mounted binary before deploying.
- **Codex:** `--sandbox workspace-write` with
  `sandbox_workspace_write.network_access=false` and
  `sandbox_workspace_write.exclude_slash_tmp=true` (codex-cli 0.167.1 accepts
  both under `--strict-config`); its shell inherits no environment and gets the
  shims' `PATH` and the workspace's `TMPDIR`. Codex has no turn limit.
- **Environment.** Cleared, as for answer runs: the login only, the `PATH`
  above, the workspace's `TMPDIR`. No database URL and no AQ credential reach
  the harness.
- **Profiles.** Agent work keeps only a profile's logged-in candidates; a
  profile with none fails the request at once (`no_candidate`, "agent work
  needs a logged-in harness, and intelligence profile … lists none").
- **One agent at a time.** A host runs at most one agent run, so one of its
  two slots always serves answer tasks. A Codex agent run holds the Codex login
  for its whole run (up to 30 min), and it is claimed only while no blocking
  answer task ends on Codex. While it runs, blocking tasks with a later
  candidate skip Codex (`login_busy`); a task whose only candidate is Codex
  waits. A Codex-first agent task can itself wait up to its deadline while
  blocking tasks that end on Codex keep arriving, without trying its later
  candidate. Prefer a Claude-first profile for `playbook_compilation`.
- **Time.** Agent deadlines may be up to 1 800 000 ms. A candidate's budget is
  what is left less 300 s for each later candidate, at least 300 s. At the stop
  the whole process group is killed, then every process of 10002, a Codex login
  is still copied back, and the attempt says "`<harness>` did not finish within
  N s".
- **Transcript.** Each candidate's tool calls, tool results and text are kept
  in `internal_task_transcripts` (items at most 2 KiB, at most 256 KiB per
  candidate) and shown in the run read. `internal work: a transcript was not
  recorded` means that write failed. Runs and transcripts are to be a rolling
  set (spec 0012 §4, pending); today nothing deletes them.
- **Redaction.** Whichever harness ran, every stored transcript, outcome and
  failure detail has the secrets of both logins replaced: the Claude token and
  the Codex login's tokens, as staged, as left by the run and as the login file
  holds them at the end (a refresh copied back meanwhile included).
- **Disk.** Nothing bounds what an agent writes into its workspace in 30
  minutes; give the `scratch` volume a quota if that matters.
- **Deploy order.** Migrate the server (0123) before starting a host that runs
  agent work, and update **every** internal work host before anything submits
  agent work: an older host claims agent tasks and runs them as one prompt with
  tools off, so its "outcome" is free text.

### What the image and compose need for agent work

- The image (Task C3 builds it): the engine CLI at
  `/usr/local/libexec/aq-playbooks` (root-owned, not on `PATH`), `/bin/sh` and
  `/usr/bin/env` for the shim. No passwd entry is needed for 10002.
- Compose: nothing new beyond the `internal-work` service's
  `cap_add: [CHOWN, DAC_OVERRIDE, FOWNER, KILL, SETGID, SETUID]` (SETUID and
  SETGID cover 10002; the sweep itself runs as 10002),
  `init: true` (reaps what the sweep kills) and a private PID namespace. The
  `homes` and `scratch` volumes must be traversable by both uids (root-owned
  0711 or 0755, never group or world writable). Keep `/tmp` a tmpfs.

### What an agent run can reach

In plain terms:

- **A Claude agent** reads and writes files only in its workspace
  (`--restricted`) and runs only `aq-playbooks …`, through the shim, which
  never sees the token. What `aq-playbooks` itself may touch is the engine
  CLI's contract (workspace-relative paths only).
- **A Codex agent's shell** can read whatever uid 10002 can read, its own Codex
  login file (`$CODEX_HOME/auth.json`) included, and can write only its
  workspace; it has no network. It cannot read other runs: they belong to
  another uid.
- **Neither** can reach the database, the AQ server or any AQ credential: the
  environment is cleared and none is mounted.
- **The provider sees what the agent reads**, its own login included for Codex.
- **Redaction** replaces exact login secrets. A secret an agent deliberately
  splits, encodes or reverses is not matched.
- **The outcome is untrusted** until its caller checks it: it is whatever the
  agent wrote (a Claude agent can `Write` it without the engine CLI), checked
  here only against the answer schema. Playbook authoring re-checks it on the
  server (Task C4).

Only a real login can confirm two things, before agent work is enabled:

1. Claude enforces `Bash(aq-playbooks:*)` against compound commands (`&&`,
   `;`, pipes), command substitution and redirections under `dontAsk`.
2. The Codex sandbox (bubblewrap with user namespaces) starts inside this
   hardened container. If it cannot, Codex agent candidates must fail fast with
   a clear attempt message so that the next candidate runs; do not turn on a
   weaker sandbox mode instead.

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
