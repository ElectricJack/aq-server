---
title: Keep large observation payloads out of weekly usage aggregates
author: Jack
date: 2026-11-25
branch: perf/integrate
id: l-4cabef7d
scope: repo
status: active
src: agent
touches:
  - crates/aq-server/src/dashboard_api/performance/usage_attempts.sql
  - docs/implementation/fleet-performance-qualification-2026-11-24.md
---

## Decided
Project the attempt identifier and unresolved flag before grouping retained observation events for fleet reports. Check the result set and execution plan against a restored database before accepting weekly reporting changes.

## Tried and ruled out
Grouping the event rows looked inexpensive with synthetic fixtures. On a restored PostgreSQL database, grouping the wide JSON rows spilled 4,543 temporary blocks. An alternating comparison returned the same 443 attempts and reduced p95 from 682 ms to 357 ms when the narrow projection was materialized.

## Watch out
SQL execution time measures one part of a dashboard read. Exercise the scoped HTTP endpoint too, including unknown pricing and timing coverage.

In the code: crates/aq-server/src/dashboard_api/performance/usage_attempts.sql and docs/implementation/fleet-performance-qualification-2026-11-24.md.
