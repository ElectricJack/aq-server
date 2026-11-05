# Retire a stopped, unsettled attempt

An installation owner can terminally abandon a profiled attempt whose actual
stop receipt is recorded but whose result cannot be settled. This includes old
stopped attempts left holding a task before this endpoint was added. Schema 66
is required; no background reaper or current worker connection is needed.

Send an authenticated, CSRF-protected request:

```http
POST /api/execution/attempts/{attempt_id}/disposition
Content-Type: application/json
X-AQ-Request: 1

{"operation_id":"<new UUID>","reason":"Retire failed qualification run"}
```

The response records `disposition: "abandoned"`, the attempt and task IDs,
the authenticated owner ID, reason, exact stop receipt digest, and server time.
The immutable disposition and `holds_task=false` commit together. Launch access
is revoked. Repeat the same operation and reason to recover the identical
receipt after a lost response or server restart; changing the operation, reason,
attempt or owner is a conflict. An owner can retrieve the receipt with
`GET /api/execution/attempts/{attempt_id}/disposition`.

This is a **terminal server disposition**: AQ will not automatically offer the
same Graph task ID again. The endpoint does not change Graph's `task_state` or
claim successful completion. Source retention and effect records stay intact,
including any unresolved obligations. Use the existing replacement path when
a retry with retained source and reconciled effects is wanted instead.

Running attempts, missing stop proof, and attempts with accepted or admitted
Graph completion are refused with 409 and retain their claims. A stop report by
itself continues to preserve the claim until settlement, safe replacement, or
this explicit owner action. The endpoint is unavailable to workers and viewers.
