---
title: Give test shards the runner that built their archive
author: Jack
date: 2026-11-21
branch: fix/reuse-nextest-for-ci-shards
id: l-3e65db33
scope: repo
status: active
src: agent
touches:
  - ".github/workflows/ci.yml"
---

## Decided
Store the pinned cargo-nextest executable with the archive produced by the build job. Each shard makes the downloaded copy executable and invokes it from the same run's artifact. The six shards keep the same test selection and permissions.

## Tried and ruled out
A real AQ delivery was returned for rework when shard 3 could not download the runner. The install action retried the download eleven times and received HTTP 403 each time, while the other five shards installed the same release. The shard's test command was skipped. Changing the task source or spending another sandbox budget could not fix that failed download. A six-shard local replay with the pinned binary and a downloaded artifact's ordinary file permissions executed all 54 fixture tests.

## Watch out
The build job still depends on obtaining the pinned release. A failure in the build job needs separate diagnosis. Inspect the failed step before attributing a red check to the test suite.

In the code: .github/workflows/ci.yml, jobs.tests-build and jobs.tests-shard.
