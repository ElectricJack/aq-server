# Native production runner: next bounded delivery

Baseline: agent-q `1b7e4f5` / state 33, paired with the current native worker
components. This is an implementation delta, not runtime/model qualification.
No provider request is part of this increment.

The daemon's phase pipeline is usable by the controlled worker process, but the
ordinary worker executable has no native run command. `RunExecution` uses the
legacy synthetic protocol. Native launch is compiled only for tests/test-support
and accepts `SyntheticHostProfile`, a closed small-ELF adapter with a 4096-byte
startup ceiling. Removing that guard or renaming the profile would not produce a
qualified real-agent path.

## First worker increment

Add a foreground native runner and an installed-adapter boundary, using existing
native journal, source/material preparation, exact Grant, Started/Stopped and
output owners. Validate the private installed configuration and supported adapter
before registration/polling. Missing executable pins, qualification/counting/model
facts or required scopes must refuse admission; do not advertise a harness and
then consume an offer that cannot run. Keep the synthetic adapter test-only.

Use a closed adapter selection from operator-owned configuration. A task cannot
supply executable paths, arbitrary argv, shell hooks, provider endpoints or
credential values. The connector keeps its current enrolled node/service-token
credentials. The agent receives only its exact scoped provider binding and,
where qualified, its separately issued graph session credential.

The first CLI adapter must implement the existing
[worker adapter plan](../../../aq-worker/docs/implementation/WRK-08-codex-adapter-plan.md):
exact pinned executable, one final payload over a bounded stdin/file handoff,
isolated configuration, supported manifest settings, and actual observed readiness.
A wrapper PID or pre-exec marker is not readiness. Source, credential and startup
verification repeat at the actual exec boundary; output draining remains independent
of network delivery. Test with a pinned local no-provider program before any real
provider target is considered.

`LocalCommand.environment` currently participates in serialized host launch
metadata. It is not a secret handoff. The adapter should reuse `MaterialStore`’s retained private installation owner,
with the existing record, exact lease/binding/fingerprint and stopped-writer
cleanup. The first slice adds no second credential store or secret files; its
closed exec handoff must preserve that owner’s private access boundary. Never copy a
provider secret into public artifacts, argv, sealed launch metadata, logs, or the
connector's ambient environment. The worker owner should settle that concrete
exec handoff before exposing an eligible production adapter.

## Existing lease capacity, without renewal

The current 10-second execution lease is a fixture setting. Frozen admission 0.1.9
already permits an operator policy `maximum_execution_lease_ms` from 1 through
120000. A new immutable policy/qualification version may request a longer initial
lease for a bounded task. The daemon clamps it to the original execution wall and
lineage deadline; the current regression profile has a 60-second hard bound.
This changes neither an existing receipt nor its delivery clock. Original offer,
preparation and material-retrieval clocks remain immutable and may expire before
the running execution ends.

Phase terminal negotiation is explicitly `OriginalLeaseNoRenewalV1`. The legacy
Renew owner is not a phase renewal adapter. Do not let the runner silently renew,
reselect, restart, extend the hard deadline or infer an installed credential from
an expired retrieval lease. A task exceeding its initial permit stops under the
existing guardian. Longer execution needs a separately versioned continuation
owner and actual current-fact/worker integration.

Restart remains stop/preserve and authenticated historical reconciliation. A new
logical boot cannot inherit an old live Grant. Result recording, reviewed graph
settlement and verified source/effect release remain separate owners; neither
process exit nor a terminal ACK completes the task.

## Ownership and qualification boundary

Worker: command/service/config, closed installed adapter, private exec handoff and
ordinary native lifecycle orchestration. Server: retain the existing phase owner;
add no endpoint or table merely to duplicate its policy. The accompanying owner
regression checks explicit 30-second policy and 120-second policy hard clamping,
immutable original clocks, and exact Grant replay. It does not widen existing
fixtures or qualify a provider, tokenizer, sandbox, hard-cost ledger or real CLI.
