# aq-central-authority

Taskless central-session data contracts, lifecycle reducer and dormant PostgreSQL
owner. This crate never accesses secrets, starts processes or writes messages.
Runtime capabilities remain false. Model phases are reported historical data,
never process freshness. The SQL owner is not activated by the daemon.

See ../../docs/implementation/AQ-17-central-authority-core.md. Version 1 wire data
is bounded to 64 KiB; artifact refs and SessionIdentity come from the pinned public
session protocol. Central session/outer generations must match; Remote identities
and project/task/node fields are unsupported. One plan contains one immutable
mail batch and one bounded turn; no interactive injection/resume is implied.

Initial model construction is generation 1 only. Successor construction requires
bounded all-generation mail/cursor and cross-role operation history; authenticated
source ownership remains the trusted embedding's responsibility.
The schema records shapes and local bounds; always use `decode` for cross-field
validation. Even validated restored state or an Applied transition is not a grant.

## Dormant SQL owner

`owner::SessionOwner` uses migration 0045 and serializes each actor on a locked
head row. It retains at most 1024 generations of at most 64 KiB each, bounds each
operation to four seconds, and validates restored model state and identity.
Actor/conversation/authority identity cannot change. Session IDs are unique, and
only the current generation can change. Historical rows cannot be deleted.

`prepare` admits an authorized immutable plan or returns its exact current replay.
Successors require the reducer's stop/cancel gate and a `MailHistoryFence` derived
under the lock from every retained generation. Mail UUID/cursor substitution and
cross-role operation reuse refuse. `start` commits the first permit together with
the start/dispatch association; `renew` compares the prior sequence and persists
expiry even when refusing renewal. An ambiguous renewal response requires inspect
before retrying; inspection itself changes no permission. `report` cannot issue
permits and never revives expired/cancelled/uncertain execution.

Construction requires an `EvidenceVerifier`; there is no permissive/default or
production implementation. Its callbacks receive the same locked transaction so
the eventual embedding can join current actor policy, original mailbox facts,
immutable installation pins and exact physical evidence before committing.
Neither structural `StopEvidenceRef` nor `HostObservationRef` is proof on its own.
The production verifier must verify real evidence and the clock must come from
the physical host's boot, not client input. Database credentials and verifier
construction belong only to trusted central code, never a worker or sandbox.

This increment does not qualify physical stop, re-adoption, provider credentials,
mail dispatch or the supervisor binary/image. Those remain blocked on
`crisp-nexus-12`'s subject-neutral shared host contract. Historical handoff
reconciliation after a successor is not exposed by this owner yet; the current
API fails closed on stale generations. The 1024-generation limit is an explicit
capacity refusal, not an automatic retention deletion or production sizing claim.
