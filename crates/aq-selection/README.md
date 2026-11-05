# Deterministic selection

`aq-selection` is a pure library over bounded explicit snapshots. It has no
provider clients, database, daemon dependency, or credential values. Run its
qualification with `cargo test -p aq-selection --locked --offline`.

Selection preserves configured candidate order, execution harness order, and
credential profile order. Installed adapter variants have a canonical tie order.
Provider-native model and reasoning mappings must match exactly. An explicit
versioned `selection_policy` is required; old catalogs retain identical bytes and
digests, but have no inferred runnable model. Every fallback reason needs policy
permission. Unknown availability requires explicit permission and independently
present, authorized credentials; unknown never means missing credentials.

The result freezes model/provider, native reasoning, tested harness CLI/adapter,
environment and qualification artifacts, credential profile/account/endpoint,
composition digest, effective resource limits, estimates, snapshot revisions,
observation provenance and candidate reasons. It is metadata, not an offer or
launch grant. It cannot override daemon authorization or prove external facts.

Live server offers use `select_for_offer` with a 30-second selection and budget
snapshot bound (`OFFER_SELECTION_TTL_MS`) so poll, proposal and accept can tolerate
host latency. Every underlying fact still caps the deadline: node, project,
credential/preparation and influencing availability expirations remain binding,
as do source, graph and context proof expirations in offer assembly. With fresh
facts valid for at least 30 seconds, selection leaves at least 29 seconds after
900 ms of assembly latency; a five-second availability policy instead permits a
five-second window at a fresh poll. Expired facts never gain extra lifetime, and
replays retain their original absolute deadline. The `select` and
`select_legacy_for_inspection` entrypoints retain the historical two-second bound
for existing deterministic replay consumers.

Task budgets use `(project_id, workspace_id, original_task_id)`, deliberately
stronger than an individual lifecycle lineage UUID. All replacement/continuation
UUIDs must resolve to this original aggregate in authoritative central records;
worker-selected UUIDs and ledger checkpoint strings cannot reset limits. Missing
accounting is explicit, never zero. A known spent lower bound at or above a limit
refuses even under bounded unknown-cost policy. Hard cost requires both a
qualified native enforcer and an authoritative complete ledger adapter.
Replacement requires actual owning-domain stop, retained-source and effect
reconciliation receipts; DTO references alone are not verification.

`StructuredFailure` is an AQ-owned v1 classification contract. It contains no
worker-selected account, endpoint, credential or model scope. The daemon first
authenticates current node/boot and exact sealed attempt; the pure helper derives
scope from the retained selection and requires its qualified classifier artifact.
Generic lifecycle `ProviderError {code,retryable}` is unverified source evidence
and cannot directly become quota/authentication truth. No classifier is implied
by a provider name or an arbitrary worker error string.

Credential backend policy uses the closed `docker-local`, `boat`, and
`cloudflare-containers` set. Startup profiles accept `kind = "api-key"` (default)
or `kind = "subscription-login"`; kind is metadata only. Their optional
`allowed_backends` defaults to all three, with placement separately deciding
which backends are enabled. An explicit empty list denies every backend.
`CapabilitySnapshot.backend_kind` is a trusted eligibility input. Narrowed
allowlists deny missing identity with `credential_backend_unknown`; known denied
backends produce `credential_backend_not_allowed`. Both are `PolicyDenied` and
obey the configured fallback policy. Default snapshots keep their historical
serialized form. Existing server producers supply unknown until C4 connects
authenticated advertisements; restricted profiles therefore fail closed there.
The material provisioning and issuance helpers require the same explicit input.
