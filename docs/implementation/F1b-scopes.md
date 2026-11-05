# Supervisor credential lifecycle and route scopes

Task `steady-harbor-87.2.3`, based on the supervisor-authorized F1b checkpoint
`b807f893fa50089c1b7108168a3a4a4faa5973aa` from `aq/steady-harbor-87.2`.
The merge of `origin/main` at `d616375f` retains reservation consumption as
migration 0049. Operator coordination reserves migration 0050 for B2; this
follow-up uses migration 0051 and state format 51. It depends on
`noble-summit-37.2`, stacked at published commit
`1e3b87c81d6f3d7268062f26a025666f9343aff4` to preserve contiguous migrations.
The subsequent clean merge at `1b570310` preserves host credential activation;
the named owner-message/reply acceptance check is rerun against that wiring.
The final build-hygiene merge preserves the current vendor exclusions, pinned
image bases and image-label checks; both label checks pass at format 51.

AQ's publication identity guard required replaying the prerequisite merges under
the project committer identity. Original B2 checkpoint `1e3b87c8` corresponds to
`4f6d7085` in this branch; its author and tree are unchanged. The complete checkout
tree before and after that repair was identical
(`c615f5c91dc032ab7401d482122fd6f6725ffd2a`), preserving the recorded test results.

## Credential and reply transactions

`reply::authenticate` preserves the existing generation-bound principal and
401/403 behavior. `reply::authenticate_in_transaction` performs the same check
inside the caller's transaction. It locks the central actor head, validates the
retained state through `SessionOwner::inspect_in_transaction`, and then locks and
re-reads the exact credential. The actor and credential locks remain held through
commit. All lifecycle writers and credential issuance take the actor lock first.
Issuance now checks the active phase inside its own write transaction as well.

The reply handler authenticates `messaging` before parsing and loading the
association, then authenticates the exact token again inside the reply write
transaction, before conversation locks, replay lookup, retention charges or
mailbox writes. A stop, loss, replacement credential or successor between the
first check and the write cannot admit a reply. After the second check, lifecycle
and credential changes serialize after the protected transaction commits.

Migration 0051 explicitly sets `revoked_at_ms` in the same transaction that changes
a generation to an inactive phase. This includes stop requested, stop uncertain,
stopped, cancelled before start, permit expiry and HostLost, regardless of whether
the transition came through the host controller or the central owner. The first
revocation timestamp is retained. The migration also revokes previously retained
credentials whose generation is inactive or no longer current. The format version,
readable formats and Docker default advance together to 51.

## Route integration contract

At the F1b checkpoint the only supervisor machine route is
`POST /v1/supervisor/conversation/replies`. F2/F5 graph, enqueue and decision
supervisor routes do not exist yet. This change does not implement those features
or extend worker/owner routes to accept a fabricated worker identity.

When those handlers land, call `reply::authenticate(pool, bearer, required_scope,
at_ms)` before dispatching, and use `reply::authenticate_in_transaction` in the
transaction that reads or changes protected state. Keep that transaction open
through the protected operation; an earlier principal alone is insufficient.

| Operation | Required scope |
| --- | --- |
| Graph read | `graph.read` |
| Graph mutation | `graph.write` |
| Enqueue task or graph | `enqueue` |
| Reply or mailbox message | `messaging` |
| Decision operation | `decisions` |

A credential missing the required scope returns 403; an expired, revoked or
inactive-generation credential returns 401. Enqueue and decision routes must not
substitute `graph.write` for their dedicated scope. These future route owners must
add route-level tests when their routes exist. The shared authenticator is tested
with all five scopes and each single-scope credential denied all other scopes.

## Verification

Checks run through AQ's foreground resource gate against the disposable local
PostgreSQL database:

```
F1B_SCOPE_CHECK=<check> DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres \
  aq test docs/implementation/F1b-scope-evidence/test_cargo.py -q --timeout=1800
```

The wrapper bounds Cargo to two jobs and test threads, disables incremental
compilation and writes each check's exact Cargo output to
`F1b-scope-evidence/<check>.txt`. See `test_cargo.py` for repeatable commands.

| Check | Result | Evidence |
| --- | --- | --- |
| Credential scopes, lifecycle, reply race, locks and upgrade | 5 passed | [credentials.txt](F1b-scope-evidence/credentials.txt) |
| Central sessions, supervisor conversation and turn consumption | 20 passed | [area.txt](F1b-scope-evidence/area.txt) |
| Host owner-message/reply acceptance | 1 passed | [acceptance_host.txt](F1b-scope-evidence/acceptance_host.txt) |
| State-format/migration consistency | 1 passed | [format_version.txt](F1b-scope-evidence/format_version.txt) |
| Docker state-format and protocol labels | 2 passed | [image_labels.txt](F1b-scope-evidence/image_labels.txt) |
| Rust formatting | Passed | [format.txt](F1b-scope-evidence/format.txt) |

The five credential tests use real PostgreSQL transactions with synthetic lifecycle
evidence. They add no sleeps or physical-host dependencies. The lock test uses
`NOWAIT` to prove that both lifecycle and credential writers are excluded until
the authenticated operation commits.
