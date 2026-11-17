---
name: aq-cli
description: Check AQ Platform v2 health, discover commands, and use its CLI for tasks, projects, workers, and other operations from a host or AQ sandbox.
---

# AQ Platform v2 CLI

On a host, use `aqv2`. Inside an AQ v2 sandbox with `AQ_CHANNEL` set, use `/opt/aq/bin/aq` with no host connection flags. A host command named `aq` may still resolve to the v1 Python CLI. In particular, `aq status` can report a stopped v1 daemon while v2 is healthy.

## Local host

Use the stack's configured development origin. On the `aq-ms1` desktop stack it is `http://127.0.0.1:28080`.

```sh
curl -fsS --max-time 5 http://127.0.0.1:28080/healthz
aqv2 --local --origin http://127.0.0.1:28080 commands
aqv2 --local --origin http://127.0.0.1:28080 projects list
aqv2 --local --origin http://127.0.0.1:28080 tasks list
aqv2 --local --origin http://127.0.0.1:28080 workers list
```

`--local` uses the loopback server's configured development identity. For an HTTPS deployment, use its admin origin and `--cf-access-token-file /absolute/private/access.jwt` instead of `--local`. If `aqv2` is absent, install a matching release binary as `aqv2`, or run `cargo build --locked -p aqv2` from the server checkout and use `target/debug/aqv2`.

## Command discovery and results

- Use `commands` for available commands and `commands --all` for scopes and refusal reasons. Typed commands also have `--help`.
- Use `run NAME --describe` to inspect a built-in command's parameters without connecting. Invoke a registered command with `run NAME --param KEY=VALUE` or `--params-file FILE`.
- Host examples include `tasks show TASK_UUID --project PROJECT_UUID`, `tasks list --project PROJECT_UUID`, and `chat`. In a sandbox, `aq whoami` shows the current task, project, and grants.
- Use `--json` for the CLI envelope and read `data` on success or `error` on failure. List commands can return cursors for subsequent pages.
- If a mutation returns an operation ID and an uncertain outcome, inspect it with owner `operations show ID` or sandbox `operation show ID` before considering another submission.
