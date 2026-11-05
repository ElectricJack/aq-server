# Project execution bindings (AQ-19)

AQ owns revisioned project execution policy; aq-git owns repository bindings and
generations, and aq-graph owns workspace/task state. Project records store public
references, allowed profile identities, central node/group/environment restrictions,
policy/context revisions and integer resource/cost ceilings. They contain no task
lifecycle, copied repository metadata, remote URL, filesystem path or secret.

Owner routes use the existing authentication, CSRF and maintenance controls:

| Route | Contract |
| --- | --- |
| GET /api/projects?after=UUID&limit=50 | Bounded UUID-cursor page (limit 1..100) |
| GET /api/projects/{id} | Current revision and binding |
| GET /api/projects/{id}/revisions/{revision} | Immutable retained historical revision |
| POST /api/projects/{id} | `{operation_id, expected_revision, binding}`; zero revision creates |
| POST /api/projects/{id}/disable | `{operation_id, expected_revision}`; no external dependency |
| POST /api/projects/{id}/eligibility | Expected project/catalog revisions, node/environment and exact agent/intelligence/execution profiles |

Mutation UUIDs are actor scoped and bound to exact typed payload digests. Same-ID
identical retries return the original receipt; changed payloads conflict. CAS
updates append immutable history and receipt with the current projection in one
transaction. Validation failures leave the previous binding intact. Definitions
remain editable through these central operations only. Reads and eligibility do
not create grants, offers, attempts, worktrees, graph records or Git bindings.

Bindings must name fixed logical adapter references `aq.git.configured` and
`aq.graph.configured`, backed by existing server-owned aq-git socket/config and
graph Runtime endpoint/token. Callers cannot provide another endpoint, token,
credential filename, remote URL or arbitrary command. Supported initial repositories
are existing central-mode SHA-1 aq-git bindings. Public `repo.show` uses protocol 1,
expected binding generation and a five-second deadline, with a 64 KiB response cap.
The graph adapter uses public workspace-scoped `GET /nodes?limit=1`, API version 1,
its protected configured token, retained NodePage validation and bounded streaming.
No service-private files or graph database tables are consulted.

Creation and replacement verify the public references and active catalog before
committing. A disable operation remains available when external services are
unreachable. An external observation cannot reserve a repository generation or
change authorization. The typed eligibility seam obtains fresh public observations,
then rechecks central project/catalog revisions and node state. Local refusal of
disabled projects or disallowed node/profile/environment choices precedes external
calls. Mutation observations retain their original timestamps and exact provenance;
expiry is checked after lock waits and again before committing, never refreshed
merely because a database update completes. Observations bind
repository/workspace/adapter identity, exact generation, observation time and a
30-second maximum expiry. Absent, stale, forbidden, unavailable and unsupported
results remain distinct and cannot yield an eligible snapshot. Unknown group
references, revoked/offline nodes, profile/environment denials and activation
changes also block eligibility explicitly.

Node allowlists and `[projects.worker_groups]` membership are centrally controlled.
Worker-reported group names are never accepted. Example vault configuration:

```toml
[projects.worker_groups]
engineering = ["11111111-1111-4111-8111-111111111111"]
```

Only registered, nonrevoked node identities may be bound. Groups contain at most
256 exact UUIDs, with at most 64 configured groups. A binding contains at most 32
group references and 256 direct node IDs. Profile/environment allowlists contain
1..32 entries. Profiles are exact `id@version` references in the current activated
AQ-18 artifact. Every allowed agent's defaults must remain inside project
intelligence/execution allowlists. The initial compiler supports only those exact
default combinations; the eligibility seam refuses unrelated allowed combinations.

Permissions are exact logical operation/scope pairs (at most 128), never a grant
by possession of a recipe. Cost ceilings identify currency and integer microunits;
requiring hard cost enforcement is policy, not proof that a harness supports it.
AQ-20 must refuse incompatible limits and consume shared lineage accounting.
Context is bounded to 1 MiB, wall time to seven days, positive memory/CPU limits
fit signed 64-bit units, and process ceilings are 1..65,536. Policy/context sources
are versioned digest references; context bodies remain with their owning services.

`FrozenProjectBinding` is the per-attempt seam for AQ-20/AQ-08: exact project
revision, catalog activation revision, requested eligible combination and external
observation provenance. Consumers must obtain fresh external eligibility, then call the final
`fence_for_offer` inside the *same transaction* that inserts the profiled offer,
retain its locks through insertion, recheck time before the final commit/effect,
and durably persist the frozen project snapshot/digest before granting execution.
The final fence performs no external I/O and locks in canonical order: node,
project, catalog registry. Merely calling the asynchronous inspection helper again
outside that transaction is insufficient. Updates/activation never rewrite an existing snapshot or running attempt.
The legacy fixture execution protocol has no fabricated profiled admission path.
AQ-08/AQ-20 runtime consumers remain separate tasks; an eligible inspection is
neither a provider selection nor execution authority.

A project's source policy is either `Immutable` (an exact object ID) or
`NamedRef` (a validated `refs/heads/…` or `refs/tags/…` name). The native offer
producer admits both (`profiled_execution/producer/workflow.rs`). For a first
attempt, the producer observes the source once through aq-git, before deriving
the export operation:

- **Immutable** keeps its historical export operation key: project, task
  revision, project revision, generation and policy reference.
- **NamedRef** resolves the ref once at offer time and appends the resolved
  commit to that key. The commit is frozen into the export. A moved ref is a
  different export operation, never the old operation following the ref.

The producer observes the source again before it assembles and commits the
offer. A ref that moved between the two observations refuses that offer, and no
offer is committed. The next poll resolves the ref again and freezes the new
commit under a new export operation. Once an offer exists, it and its
preparation use only the frozen exported commit. A replacement attempt reuses
its predecessor's verified export and retained commit, and it never resolves the
configured ref again.

Admin requests are capped at 128 KiB. Four project verification exchanges may run concurrently (each pairs one
bounded Git read and one graph read); excess returns explicit unavailable observations instead of waiting
in an unbounded queue. Registry retention caps are 256 current projects and
100,000 historical revisions/operation receipts. History is preserved rather than
silently evicted on quota exhaustion. Project edits, binding generations and
activation revisions are rechecked rather than attempting a cross-service atomic
transaction. Any external change after observation is caught by the owner's
expected-generation checks on subsequent effects and the final admission recheck.

Routine tests use pure decisions, fake public aq-git/graph peers and disposable
PostgreSQL. Explicit qualification runs `scripts/test-project-bindings.py` against
the retained aq-git candidate image and qualified aq-graph image, exercising real
public binding reads, workspace authorization, CAS and stale-generation refusal.
It creates synthetic local resources only and does not pull/publish images or use
an operator repository, workspace or credential. Clone/init/GitHub onboarding,
worker self-enrollment into groups and per-project preallocated slots are outside
this increment.

The initial node-gated eligibility endpoint is exclusively a worker-placement
policy. AQ-17 central supervisors require their own central authority adapter;
they must never fabricate a worker node ID to satisfy project worker restrictions.
`ProjectRecord` freezes project identity/policy/ceilings independently of the
chosen worker fields in `FrozenProjectBinding`, allowing that future adapter to
reuse project facts under distinct central authority. Configured group membership
has a canonical digest in the frozen worker snapshot; final admission rejects a
membership/configuration change even when the project record is unchanged.
