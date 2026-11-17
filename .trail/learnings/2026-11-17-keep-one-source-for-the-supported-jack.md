---
title: Keep one source for the supported v2 skill suite
author: Jack
date: 2026-11-17
branch: feat/aqv2-skill-suite
cost: { minutes: 1, tokens: 31539 }
id: l-06f2b9b6
scope: repo
status: active
src: agent
touches:
  - README.md
---

## Decided

Keep the shared suite with the repository that owns the v2 command registry because the docs and CLI can then be reviewed and shipped together. Install from a selected checkout and preserve operator edits in backups during migration.

## Tried and ruled out

A mechanical conversion of the legacy procedures would advertise unavailable features. The old Bazel upgrade skill depended on v1 runner and integration-train contracts, so leave it out of the v2 suite. Document review decisions and result feedback that v2 actually supports. A brief-to-roadmap workflow can still deliver an artifact for review even when no gate is configured.

## Watch out

The installer updates only the distributed skill entrypoints. Retiring legacy-only content is a distinct local migration. Playbook engine authoring belongs to its component repository.

In the code: `skills/aq-*/SKILL.md`, `scripts/install-aq-skills.py --dest PATH`, and `protocol/commands/v1/commands.json`.
