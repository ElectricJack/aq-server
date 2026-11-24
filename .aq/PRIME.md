> **Project note (AQ v2 platform):** this repository's default and integration branch is `main`; there is no `dev` branch, and integration is direct. Wherever the completion protocol below says `origin/dev`, use `origin/main` (for example, `git merge origin/main` as the final submission step). Do not block or ask about this.

> **Velocity rules (owner, 2026-11-02):** ship the task's acceptance criteria and stop. Verify with the targeted named tests for what you changed, not repeated full-suite or qualification runs. Do not add slow tests (real sleeps, long timeouts, real-network or full-system runs) unless the task explicitly requires one. Security or robustness hardening beyond the task's stated acceptance goes in a follow-up task, not into this task. Reviewers mark [blocking] only for defects against this task's acceptance criteria or real correctness or security bugs. Everything else is a [nit] or a follow-up.

> **Migration numbers (operator, 2026-11-03):** migrations are generated from `crates/aq-server/migrations/` and must be contiguous and unique. Immediately before pushing (and again before any direct push to main), run `git fetch origin && git ls-tree --name-only origin/main crates/aq-server/migrations/ | tail -1`. Rename your migration to the next free number, and update its `UPDATE meta SET value='N'` line and any `supported >= N` gates. If main moved after your push, renumber again. Never leave two files with the same number.

> **Build health (owner, 2026-11-05):** follow the "Build health" section of `AGENTS.md`. Build in this worktree's own target directory with incremental builds (do not export `CARGO_INCREMENTAL=0`), run every `cargo test`/`cargo nextest` under `timeout`, put new integration tests in an existing test binary (a module of `crates/aq-server/tests/<group>/main.rs`; run one with `cargo nextest run -p aq-server --test it_<group> -E 'test(/^<module>::/)'`) instead of a new test target, and keep `python3 scripts/build-budget.py` passing.

> **Sandbox limits (owner, 2026-11-24):** the task sandbox has no PostgreSQL (`DATABASE_URL` is unset), no CodeScene and no `origin/main` history, so database-backed tests, CodeScene scores and `scripts/build-budget.py` cannot run here. The pull request's required checks run the database tests and the line budget against the real `main`. Run the checks the sandbox supports, say in your result which ones it could not run, and report success when the change itself is complete. Report blocked only when you cannot finish the change. If a retry hands you a failed check's log, fix what the log names; for the line budget that means removing at least the reported number of lines from the crate's `src/`: put extracted helpers in a workspace crate that is under its target, or move inline `#[cfg(test)]` code into the crate's existing test binaries. Never edit `build-budget.toml`; raising a budget is the owner's decision.

## Role

{{role}}

{{project_role}}

## Task

{{task}}

## Task Context

{{task_context}}

## Workspaces

{{workspaces}}

## Messages

{{messages}}

{{l1_facts}}

{{l2_context}}

## Tool Guidance

{{tool_guidance}}

## Completion Protocol

{{completion_protocol}}
