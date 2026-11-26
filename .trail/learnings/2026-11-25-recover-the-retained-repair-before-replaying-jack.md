---
title: Recover the retained repair before replaying a cross-project task
author: Jack
date: 2026-11-25
branch: feat/task-project-workspace
id: l-d3be6934
scope: repo
status: active
src: agent
---

## Decided
Recover the exact retained task commit and its review constraints before using a blocked task to qualify cross-project execution. Compare the original repair with the owning source repository and run an isolated copy with the same task identity. This preserved the repair and made it possible to test the actual dashboard and server together.

## Tried and ruled out
The earlier server-only terminal attempt exercised a compiled dashboard override and recorded local test success. It could not run its PostgreSQL-backed integration case or validate browser reconnection to a real terminal. Treating that override or a mock browser test as the dashboard fix would have hidden the missing cross-repository test coverage.

## Watch out
A task's execution state, retained Git result and remote publication can have different outcomes. Check each before replaying a historic result or assuming there is a reviewable branch.
In the code: the original task was a3c72f06-d1a6-428e-bf67-b0f1c734a8c9. Its retained commit 3bed9a998cd64766f0cdcd0f0da636af21f66da3 carries docs/implementation/running-task-terminal-repair.md and the reproduced regression.
