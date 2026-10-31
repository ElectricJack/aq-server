# aq-playbooks

A standalone Rust workflow compiler and runner for repeatable project policy. AQ can embed the runner; host services retain authority over work, evidence, decisions, findings, and source control.

The workspace contains five crates:

| Crate | Responsibility |
| --- | --- |
| `aq-playbooks-model` | Versioned artifacts, canonical JSON/hashes, bounded schemas and expressions, typed named joins, structural verification, fixed-interval occurrence planning |
| `aq-playbooks-compiler` | Source/proposal validation, outcome-sensitive type checking, deterministic packaging, semantic diffs |
| `aq-playbooks-runner` | Atomic transitions, event admission, waits, effects/recovery, children/joins/cancellation, fair indexed scheduling, retention, controls, quotas, in-memory storage and simulation |
| `aq-playbooks-postgres` | Private migrations, exclusive namespace ownership, transactional snapshots/indexes, protected persistence, key rotation and immutable artifact storage |
| `aq-playbooks-cli` | Authoring tools, inspection, standalone administration, synthetic AQ catalog and four fake-host pilots |

## Try it

Rust 1.96 was used for verification. Offline commands need no database or AQ checkout.

```sh
cargo test --workspace
cargo run -p aq-playbooks-cli -- commands list
cargo run -p aq-playbooks-cli -- examples --output .local/examples
cargo run -p aq-playbooks-cli -- validate .local/examples/routing.proposal.json
cargo run -p aq-playbooks-cli -- compile .local/examples/routing.proposal.json --output .local/routing.json
cargo run -p aq-playbooks-cli -- simulate --pilot coding
cargo run -p aq-playbooks-cli -- simulate --pilot report
```

The generated examples include Markdown sources, explicit proposal bundles, canonical artifacts and scenarios. Pilots cover coding with bounded repair and independent follow-up, routing judgment, findings reconciliation, and supervisor reporting with fallback and late completion. Work creation is distinct from authoritative completion and assessment disposition. Missing mock results remain unresolved.

`commands`, `events`, and `interfaces` support `list` and `describe`. `surface` reports the twelve synthetic host commands and their finite views/templates/change variants. `diff` compares artifacts. Run `--help` for exact arguments.

## Durable administration

`install`, `activate`, `run`, `inspect`, `cancel`, `reconcile`, `controls`, `authority`, `retention`, `gc`, `retire-producer`, `metrics`, and `rotate-keys` require an explicit `--database-url` (or `AQ_PLAYBOOKS_DATABASE_URL`) and `--namespace`. Mutations acquire exclusive namespace ownership and refuse another active owner. Authority and run input are explicit JSON files. Activation and execution are separate from installation.

The standalone CLI admits runs and evaluates pure transitions. Production effects require an embedded host implementing `aq_playbooks_runner::Host`; the CLI does not invent AQ adapters. `reconcile --request` records an evidence-backed recovery request; actual readback/cancellation uses that registered host. `inspect --kind runs|effects|receipts --after CURSOR --limit N` uses bounded indexed public projections. The convenience PostgreSQL connection uses `NoTls`; hosts can supply a TLS-configured client.

Run real persistence tests against a disposable PostgreSQL 17 container:

```sh
crates/aq-playbooks-postgres/scripts/test-postgres.sh
```

The script creates and removes its own database container. See the [adapter documentation](crates/aq-playbooks-postgres/README.md) for direct database testing and measurements.

## Format and implementation status

Read the [design](docs/specs/0001-aq-playbooks.md), [implementation decisions and limits](docs/specs/0002-implementation.md), [portable format](crates/aq-playbooks-model/FORMAT.md), and [acceptance evidence](docs/acceptance.md). The [authoring skill](skills/aq-playbook-author/SKILL.md) is shipped with this repository.

The component supports fair indexed discovery, bounded safe inspection, explicit retention cleanup with replay fences, protected snapshots/artifacts, compatible named callable selection and typed named join results. PostgreSQL still commits a whole namespace snapshot, with incremental native index/projection updates; namespace size therefore remains an important performance bound. Parallel work uses owned async children/operations and joins; foreach traversal is serial. Unsupported recursive/dynamic workflows and other deferred constructs are rejected explicitly.

`Engine.metrics.snapshot()` exposes bounded process timing histograms and admission/refusal/deduplication counters for host export. `metrics` reports aggregate state gauges and retained-receipt timings; this explicit diagnostic scans the snapshot and requires keys for protected stores.

Live AQ integration remains an external dependency: the current daemon has not implemented the required domain services. See the [verified host integration boundary](docs/aq-host-integration.md). The fake-host pilots validate contracts without claiming live execution.

For confidential persistence, supply `--protection-keys /path/to/owner-only-keyring.json` consistently. The keyring selects an active key and retains old keys needed for rotation/backups; it is never stored in PostgreSQL. See the adapter documentation before rotating keys or restoring an encrypted backup. Without a configured provider, runs with sensitive command contracts and events with declared sensitive payload fields are refused before admission.

The [seventy policy examples](docs/research/2026-10-30-sdlc-policy-exploration.md) have a [capability disposition](docs/scenario-dispositions.csv). The [v1 capability selection](docs/research/0001-v1-to-v2-capability-selection.md) is supporting research, not a compatibility requirement.
