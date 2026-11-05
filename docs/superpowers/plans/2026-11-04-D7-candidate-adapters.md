# D7: Host adapters candidate.prepare / candidate.publish — plan

Task: `crisp-meadow-36.8`. Base: `f7f249db` (origin/main). Builds on D1a/D1b
(`crates/aq-server/src/delivery*`), D5 (`playbooks/host_adapters.rs`) and D6
(`playbooks/host_adapters/requests.rs`).

**Goal:** playbooks prepare an integration candidate and request guarded
publication. Publication is dispatched only when every evidence item the
project's integration policy names is present, comes from its owning service
and authorized issuer, and is bound to that exact candidate (P10).

**Out of scope:** the aq-git client and D1 intent/reconcile internals,
executing checks or judgments (D6), the coding policy (D9), GitHub
credentials (D2), wiring `DeliveryService` into `AppState` (follow-up).

## Design

1. **Port** (`host_adapters/integration.rs`). `HostPorts.integration:
   Option<Arc<dyn Integration>>`; `None` refuses `unavailable` (same pattern
   as D6 checks/judgments; production runtime stays unbound until D1 is in
   `AppState`).
   - `binding(workspace, project) -> Option<IntegrationBinding{repository_id,
     binding_generation, publication: PublicationPolicy, evidence:
     EvidencePolicy{checks, review, judgment_choices}}>`.
   - `prepare(PrepareCall{operation, repository_id, binding_generation, base,
     sources})`, `publish(PublishCall{operation, repository_id,
     binding_generation, candidate_operation, publication, expected_old})`,
     `reconcile(ReconcileCall{operation, repository_id, binding_generation,
     target_operation})`, `review(workspace, project, decision)`.
   - `DeliveryIntegration` maps the port onto `DeliveryService::
     create_candidate` / `deliver` / `reconcile` (D1a/D1b) without touching
     their internals.
2. **References.**
   - base: `[<ref>@]<sha1|sha256>:<hex>`; sources: `aq.source/<receipt uuid>`
     (1..=64, unique).
   - candidate: reference and subject id `aq.candidate/<op>`, revision 1
     (immutable); `<op>` is the prepare intent's stable operation id, which is
     also the D1 candidate operation id. A repair is a new intent → new op.
   - destination: `<refs/heads/...>@<sha1|sha256>:<hex>` (target and
     `expected_old`); the target must equal the policy target.
   - publication: reference `aq.publication/<op>`, subject = candidate subject.
   - evidence: `check-receipt:<op>` (D6 check.request), `judgment:<op>` (D6
     judgment.request), `aq.review.decision/<uuid>` (port review lookup).
3. **Evidence verification** (host, before dispatch). Each item must resolve
   in its owning store (`evidence_unknown`), come from an authorized issuer
   (check: sandbox execution profile; judgment: permitted by policy and a
   host-chosen credential profile; review: port-reported authority →
   `evidence_unauthorized`), be bound to the candidate subject and, for
   checks, the candidate OID (`evidence_stale`), and be a passing disposition
   (`evidence_failed` / `evidence_pending`). Then every requirement must be
   covered (`evidence_missing`). A policy naming no evidence refuses
   `policy_unavailable`. Other reasons: `candidate_unknown`,
   `candidate_stale`, `candidate_not_ready`, `binding_changed`,
   `destination_mismatch`, `invalid_request`.
4. **Persistence** (migration `0080_playbook_integration_receipts.sql`).
   Extends the `playbook_effects` command CHECK; new
   `playbook_integration_receipts` (operation id, intent, binding, state
   `pending|ambiguous|prepared|conflict|refused|published`, typed reason,
   request, detail, reconcile attempts). Intent (effect row: Intent.id,
   contract fingerprint, payload digest) and the pending receipt commit before
   the port is called; the final effect row, receipt and sealed
   `aq.integration.changed` event commit together.
5. **Recovery.** Prepare is keyed-idempotent: resume re-sends the same op id.
   Publish is reconcilable: a lost reply marks the receipt `ambiguous`
   (`Error::Unknown`); the next dispatch reconciles under a new op id
   (`…#<attempt>`) and never re-publishes; a resume of a still-`pending`
   publish re-sends the same op id, which D1b's dispatch fence answers
   `Ambiguous` without pushing. Settled intents return the retained result.
6. **Reads.** `operation.get` on a candidate/publication reference reports
   `running` (pending), `indeterminate` (ambiguous), or `completed` with
   `disposition` accepted/failed. `Service::integration_receipt(reference)`
   returns the typed receipt (state, reason, candidate OID, conflict detail,
   verified evidence, publication old/new).

## TDD steps

1. Write `crates/aq-server/tests/playbook_host_integration.rs` with a
   `FakeIntegration` (scripted replies, call counters) and FakeChecks:
   - `publish_refused_without_required_evidence`: no evidence, unknown /
     caller-asserted ids, a stale check (other subject, other OID), a failed
     check and an unrequired-only set all refuse with their typed reason and
     never call `publish`.
   - `prepare_then_publish_records_receipts`: prepare → `prepared`; passing
     check.request on the candidate; publish → `published`; receipts in
     `operation.get` and `integration_receipt`; two `aq.integration.changed`
     events; re-dispatch returns the retained result with no second call;
     an ambiguous publish reconciles without a second publish.
   Run: fails to compile (no port).
2. Migration 0078; constants (`HOST7_COMMANDS`, `HOST7_CAPABILITIES`);
   `HostPorts.integration`; `Service::new_host7_integration`.
3. `integration.rs`: port, references, prepare/publish adapters, evidence
   verification, settle, operation.get extension, inspection.
4. Green: `cargo test --locked -p aq-server --test playbook_host_integration`,
   then `playbook_host_requests` (constant changes), fmt, clippy.
