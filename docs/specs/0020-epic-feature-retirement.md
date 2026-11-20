# 0020 Epic feature flags and retirement

An epic groups tickets. Each implementation task delivers independently under [0019](0019-integrated-delivery.md). A compile-time flag keeps incomplete behavior off in ordinary builds. The delivery playbook controls approval and cleanup. The code exposes task, dependency, decision, build and delivery evidence.

## Register the flag and its cleanup together

When the first task introduces an epic flag, record the epic ID, affected repository, flag spelling, ordinary-build default, expected permanent behavior and accountable project. Keep one logical flag per independently releasable epic with per-repository names. Use the same registration identity on replay. For Epic 0 the Rust name is delivery_playbook and the dashboard name is VITE_DELIVERY_PLAYBOOK.

Ensure one final cleanup task for that epic and flag generation, with the registration identity as the stable work key. Record it as required for epic completion. On partial creation, reconcile until the same task and dependencies exist. A flag-introducing PR must reference that registration and cleanup task. Add newly affected repositories to the same record. Restrict the cleanup task's prerequisite to the approval of the integrated feature. Making approval depend on completion of the epic itself would deadlock.

## Verify and approve

Each repo builds with all active flags off, each alone, and all on, deduplicating identical configurations. Keep the old implementation behind the disabled path only when compatibility requires it. Exercise the integrated feature with its flag enabled. Record the vetted revisions or immutable builds across participating repositories, test evidence, decision authority and decision revision. An authorized approval releases the existing cleanup task once. A request for changes leaves it blocked. Re-vet material changes to the approved implementation; unrelated movement on main does not invalidate approval.

## Remove and verify

The cleanup agent makes the approved path permanent and removes the flag declaration, build wiring, conditional code, superseded behavior and obsolete CI entries. Coordinate multi-repo changes in compatibility order. Record each PR and merged SHA. The ordinary build of every affected repo must pass without supplying the retired flag. Run an integrated acceptance check on the combined resulting builds. Search executable code, manifests and active workflows for surviving references, allowing historical records to retain the name. Other active epics keep their independent flags and tests.

The epic playbook can complete the epic only after the authorized approval, every required cleanup merge, passing ordinary-build evidence and the integrated check. A failed cleanup returns through the normal PR repair flow. Report the outstanding task, repository or failed check on the epic. Show approved work awaiting retirement as “Approved · flag removal pending”. Replay must recover the same approval, cleanup task and receipts across restarts.

## Acceptance and work order

1. Register A and B in the same repo. Retry A's registration during partial failure. Exactly one cleanup task exists for A and one for B. The ordinary build keeps both incomplete features off.
2. Finish and vet A while B is in progress. Reject A once, then approve its corrected integrated revision. Duplicate approval delivery after restart releases only A's task.
3. Merge A's first cleanup PR in a two-repo epic. The epic remains open. Fail the second PR, repair it and verify the combined ordinary builds. A becomes the default, A's flag disappears, and B continues working both on and off.
4. Implement registration and stable task identity first, then decision release and closure evidence, followed by CI inventory and dashboard status. Pilot retirement on a scratch flag before removing Epic 0's real fallback.
