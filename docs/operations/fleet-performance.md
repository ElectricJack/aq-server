# Read fleet performance

Open **Performance** in the AQ dashboard using an owner account. Start with the seven-day delivery summary, then compare it with the 24-hour machine timeline and the current holds. The first release uses AQ and PostgreSQL. The server advertises the optional `performance.metrics.v1` dashboard capability. Older dashboard bundles can continue to use their existing endpoints.

## Interpret the numbers

- **Delivery.** AQ completion and verified target-branch merge have independent clocks. For a project with no reporting definition, both observations remain visible and configured delivery reads “not tracked.” Timing coverage is the number of observations with a verified first-ready time. Historical tasks may have no such time. A worker reporting success is a submission.
- **Machines.** The ready line is a fleet count. An idle machine alongside queued tasks is a prompt to inspect eligibility, including capability, quota, reservations and owner availability. A truncated ready page supplies a lower bound. Missing minute samples or stale heartbeats break the idle interval. Samples accumulate after rollout.
- **Queue diagnostics.** Reason counts cover observed scheduler decisions. Timing starts at the final observed eligibility gates and ends when AQ confirms the durable claim. This clock excludes earlier unobserved waiting. Use the count of covered claims and dropped observations when comparing periods.
- **Holds.** Hold age starts at entry into that hold. A reminder can change the next scheduled check without resetting age. These are live holds even when an earlier cutoff has been selected for outcomes or usage.
- **Usage.** Follow a task to see actual models and attempts alongside the original routing choice where known. Compare costs only within one currency and basis. A token-rate estimate can accompany a subscription run. Missing token categories, missing prices and unmatched overhead limit how much can be inferred from the totals. An attribution gap leaves the usage unassigned.

Use `aq performance delivery show --help` and `aq performance delivery set --help` against an updated server to inspect or change a project's reporting preference. The command registry identifiers are `performance.delivery.show` and `performance.delivery.set`. Read the current revision before setting the new one. A project default applies unless a work-class override is in force. Changes apply by first-ready time. A null definition restores inheritance. The settings control reporting and confer no scheduling or merge authority.

## Collection and compatibility

The sampler records one UTC minute per machine and one fleet demand reading. Seven days of history are retained. Historical outcomes, holds and accounting reads use their existing durable owners. Task reads and scoped summaries filter by project. The fleet inventory and scheduler examples require fleet inspection access.

To disable capture on the next server startup, set `AQ_PERFORMANCE_SAMPLING=off` for occupancy and `AQ_PERFORMANCE_SCHEDULING_DIAGNOSTICS=0` for scheduler observations. Reads remain available for retained observations. The page contains no prompt or transcript bodies.

The server migration is state format 178. Before upgrading an existing server, follow the installation's normal database backup and format-compatibility procedure. Roll out the server before the dashboard. Switching off collection is a forward change that preserves task history. A server that only understands format 177 requires a compatible database for rollback. Operators should account for writes made after a backup before considering restoration.
