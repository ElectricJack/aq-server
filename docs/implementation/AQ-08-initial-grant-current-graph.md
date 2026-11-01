# Fresh Graph observation at initial grant

The original grant checkpoint validates bounded original read provenance but
cannot detect a task changed while that observation remains unexpired. Initial
Prepare and exact Prepare replay now perform new bounded ready/context reads
between the existing two SQL phases. No SQL transaction spans public Graph I/O.

The trusted original typed spec pins exact task CAS/content revision, project and
workspace. The ready page must contain that exact active task, and ready/context
root records must agree. The current authorized ContextSnapshot must equal the
original retained startup projection, including all nodes, edges, pinned revisions
and coverage. Only the read's own observed_at field is excluded from comparison;
the stored original bytes, digest, stamps, offer, material leases and selection
are never rewritten. Unexposed private metadata does not enter startup. Missing,
changed, inaccessible, incomplete or unavailable observations refuse admission;
unavailable transport is reported as unavailable and retains ownership.

The fresh observation is tied to the exact original spec/context digests and
checked after final fences, quota waits and all later SQL writes/reads immediately
before commit. New grant expiry is additionally capped by both fresh read expiry
bounds; exact replay cannot return an original permit beyond the current ceiling.
Existing project/registry/material/Git and immutable hard deadline fences remain.
No fresh Graph result is itself an execution grant or source/claim release proof.

The first profile retains publication's bounded ready page8 and context depth1,
64nodes/1MiB limits. A task displaced beyond that page conservatively refuses; no
unbounded scan is added. Two services do not share an atomic transaction: these
are fresh finite observations, not a guarantee against a Graph change after the
read or after commit. Runtime renewal/cancellation and outcome CAS checks remain
separate required protections. No graph lock is claimed across worker execution.

Qualification exercises changed CAS, mismatched ready/context content, absent
readiness, changed authorized content with unchanged CAS in both responses, a
private-metadata-only success, immutable original publication and changed-context
replay refusal preserving the held grant. Existing concurrency, rollback, expiry,
private-material and actual-Git cases remain in the selected regression suite.
The deterministic test clock now never lags real public read timestamps; injected
expiry still advances to the original deadline without widening it. Frozen
qualification results and source hashes are recorded separately after checks.

Frozen source `421804a` passed43 tests across five targets, including15 initial
grant cases with two actual-Git cases, with no failures or ignores. Strict affected
all-feature Clippy and workspace formatting passed. [Evidence inventory](evidence/aq08-current-graph-421804a/artifact-sha256.json)
binds source and logs. The earlier15-case preflight precedes the review-driven
final precommit check and expiry cap; only the final43-case run qualifies this
source. Counts overlap earlier component checkpoints.
