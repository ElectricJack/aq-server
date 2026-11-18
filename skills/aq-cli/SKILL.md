---
name: aq-cli
description: Check AQ Platform v2 health, discover commands, and use its CLI for tasks, projects, workers, and other operations from a host or AQ sandbox.
---

# AQ Platform v2 CLI

Use `aq` for AQ v2. Inside an AQ v2 sandbox with `AQ_CHANNEL` set, use `/opt/aq/bin/aq` without host connection flags. Use `aq1` for the legacy Python CLI on a host with both versions. Check v2 health with `/healthz`. The `aq1 status` command checks the v1 daemon.

## Local host

Use the stack's configured development origin. On the `aq-ms1` desktop stack it is `http://127.0.0.1:28080`.

```sh
curl -fsS --max-time 5 http://127.0.0.1:28080/healthz
aq --local --origin http://127.0.0.1:28080 commands
aq --local --origin http://127.0.0.1:28080 projects list
aq --local --origin http://127.0.0.1:28080 tasks list
aq --local --origin http://127.0.0.1:28080 workers list
```

`--local` uses the loopback server's configured development identity. For an HTTPS deployment, use its admin origin and `--cf-access-token-file /absolute/private/access.jwt` instead of `--local`. Install the `aqv2-x86_64-linux` release binary as `aq` on an x86-64 Linux host. To build from the server checkout, run `cargo build --locked -p aqv2` and install `target/debug/aqv2` as `aq`.

## Command discovery and results

- Use `commands` for available commands and `commands --all` for scopes and refusal reasons. Typed commands also have `--help`.
- Use `run NAME --describe` to inspect a built-in command's parameters without connecting. Invoke a registered command with `run NAME --param KEY=VALUE` or `--params-file FILE`.
- Host examples include `tasks show TASK_UUID --project PROJECT_UUID`, `tasks list --project PROJECT_UUID`, and `chat`. In a sandbox, `aq whoami` shows the current task, project, and grants.
- Use `--json` for the CLI envelope and read `data` on success or `error` on failure. List commands can return cursors for subsequent pages.
- If a mutation returns an operation ID and an uncertain outcome, inspect it with owner `operations show ID` or sandbox `operation show ID` before considering another submission.
