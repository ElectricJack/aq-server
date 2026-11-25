# AQ fleet performance and model efficiency

**Design proposal for owner review · 24 November 2026**

AQ operators need to answer three questions from a single Performance page. How quickly does ready work reach the target branch in good condition? When delivery slows, which tasks, workers, or gates account for the delay? Does delegating to cheaper models reduce the total cost of delivering successful work?

## Architecture decision

Use PostgreSQL and AQ’s existing accounting crate for task outcomes and costs. Use Prometheus for operational time series, recording rules and alert evaluation. Use Perses for the fleet charts. The AQ dashboard owns task and attempt drill-downs, permissions, and the scorecard. A downstream installation can enable the monitoring services locally. Execution and delivery continue if either monitoring service is unavailable.

| Layer | Owns | Reuse |
|---|---|---|
| Workers and controllers | Timestamped observations of slot state, model usage, source identity and reporting gaps | Existing heartbeats, session observations and supervisor reports |
| AQ server and PostgreSQL | Task timelines, eligibility, revisions, task cohorts, cost attribution and quality outcomes | Existing attempts, hold and review records, accounting projections and forge merge evidence |
| Prometheus | Rates, occupancy history, duration histograms, alert rules and collector health | Apache-2.0. [Project and license](https://github.com/prometheus/prometheus) |
| Perses | Fleet timelines and operational charts with version-controlled definitions | Apache-2.0. [Project and license](https://github.com/perses/perses) |

Use OpenTelemetry semantic conventions where they match an observed model call. Pin the convention version in the adapter. OTLP tracing can be added independently. Langfuse’s MIT core is a future option if operators need model-call inspection or evaluation datasets. Neither is a deployment prerequisite for the scorecard. [OpenTelemetry GenAI metrics](https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/) · [Langfuse licensing](https://github.com/langfuse/langfuse)

## Measurement contract

A task enters its cohort on its **first authoritative ready transition**. Freeze project, work class, intended outcome and applicable delivery target at that point. Record the initial routing policy and its version when routing first occurs. Retries, reassignment, returns and holds share the original task clock. Group by project and intake class before comparing policies. Historical tasks whose first-ready time cannot be recovered appear under unknown coverage.

Use UTC, half-open intervals and an explicit report cutoff. Count each task once. For code work, delivery requires verified merge evidence on the intended target branch. If a task requires multiple PRs, use the last required merge. Use forge merge time when verified, otherwise label the timestamp as AQ’s first verified observation. Track reconciliation delay separately wherever both timestamps exist. For a document or decision, use its accepted result and display a separate cohort. A worker saying “done” records a submission.

| Scorecard measure | Definition |
|---|---|
| Delivery speed | Median and p90 elapsed time from first ready to verified delivery among delivered tasks, with sample size. Alongside it show age of open work, arrivals, deliveries and backlog past target. |
| On-time delivery | Tasks delivered before their class deadline divided by all tasks in the ready cohort whose deadline has elapsed. Retain cancelled and superseded work as explicit outcomes. List younger cohorts as pending. |
| First-pass acceptance | Among tasks with a first adjudicated submission, fraction accepted without substantive human or agent correction. Show undecided submissions separately. Link the acceptance or return to the submitted revision. A mechanical green branch refresh retains its submission identity. |
| First correct submission | For a delivered task, elapsed time from first ready to the earliest submitted revision accepted into the delivered change without substantive correction. Show unknown when revision ancestry or review disposition cannot establish the link. |
| Escaped defects | Share of delivered code tasks with a linked revert, rollback or confirmed corrective change within 14 days of merge. Count a task once and show severity separately. Report the mature sample, pending observation windows and coverage of defect linkage. A later commit touching the same file alone establishes no defect link. |
| Efficiency | Total attributable model cost in a comparable ready cohort divided by successful deliveries in that cohort. Include failed, retried, escalated and abandoned work. See the cost policy below. |

Show calendar throughput by merge date separately from ready-cohort delivery rates. For example, a merge this Monday can improve this week’s throughput and last week’s ready cohort. Dashboard tooltips identify the clock and denominator.

## Waiting, holds and resource utilization

Persist stage transitions for admission, claim, startup, agent execution, provider wait, supervisor intervention, review, rework, CI queue/run, green-and-approved merge wait, and merge reconciliation. Report durations and counts for each transition with sufficient coverage. Distinguish owner, dependency, quota and infrastructure waits. Show overlapping waits in the task timeline. For a ranked bottleneck chart, attribute each task-second once to the evidenced blocking gate. Use “multiple gates” or “unknown” when the evidence cannot distinguish a single cause.

A hold episode opens on the recorded hold transition and closes on release, supersession or terminal disposition. Preserve the original start when the same hold receives a reminder, heartbeat or status update. Show current age, oldest hold, hold count, union of cumulative held intervals per task, reason, responsible party, next scheduled check and last meaningful progress. A missed check and a hold past its reason-specific response target are separate measures. Calculate overdue hold-hours as the overlap of the reporting window with the time beyond each applicable deadline, unioned per task. Holds without an assigned target still appear in the age list. Supervisor recovery time runs until an action advances the task.

Show one aligned row for **each machine with a worker**, including machines with zero activity. Sum distinct worker slots by machine identity. Carry each sample forward until the next observation or heartbeat expiry, whichever comes first. Partition online slot-time into executing, provider-waiting, finishing/uploading, otherwise occupied, idle, and unknown. Display offline, owner-paused and stale periods distinctly. Report executing and occupied slot-minutes as separate percentages of online enabled capacity-minutes. Show coverage alongside each percentage. CPU and memory are drill-downs for interpreting a lull.

Calculate **avoidable idle slot-minutes** from contemporaneous eligibility. On each observed interval, match ready unclaimed tasks to compatible idle slots, respecting shared provider limits, capabilities, project constraints, reservations and owner controls. Integrate the maximum feasible number of simultaneous starts over time. Reuse the scheduler’s read-only eligibility decision at the observed policy revision. Record missing evidence as unknown. Historical matching must never claim work. One task compatible with two idle machines for ten minutes yields ten fleet-wide avoidable slot-minutes. Show both machines as eligible candidates and use the scheduler’s stable tie-break for any additive per-machine attribution.

Show the longest continuous low-execution interval on each machine and across the fleet, with duration, capacity and ready demand. Use a configurable busy fraction and minimum duration; the initial preset highlights zero executing slots for ten minutes while enabled capacity is online. Break intervals on missing observations. Classify intervals as empty queue, incompatible demand, admission-limited, eligible work waiting, or unknown. Show eligible and blocked portions separately when both occur.

## Tokens, delegation and cost

Build on the existing session usage contract and canonical accounting projections. Preserve input, output, cache read, cache write, reasoning, model, harness, billing basis, rate-card revision and reporting completeness. Carry forward v1’s by-model token rates, unattributed usage and subagent breakdown. Use v1’s token-efficiency fixtures as parity cases: deduplicate streamed Claude message IDs and repeated Codex response IDs, difference cumulative totals across verified baselines, and handle cached input already included in Codex input. Apply disjoint billing categories before multiplication. Track context compactions and polling as diagnostics where the harness reports them.

Associate each priced call once with the task lineage and consuming attempt or delegated session. Keep parent and child breakdowns reconcilable to the fleet total. Attribute escalation and repair to the **initial routing policy** for policy comparisons and to the **actual model** for consumption breakdowns. Shared supervisor activity without an evidenced task link goes into visible fleet overhead. Report attributable spend plus overhead and unassigned spend so operators can reconcile the total. A fleet-wide cost-per-success figure may include overhead once; label that basis.

For an amount in a single currency and cost basis, sum disjoint token quantities multiplied by their applicable rates, using the accounting crate’s rounding and pinned pricing rules. Display provider-reported amounts and API-equivalent or subscription-allocation estimates as separate series. Each comparison uses one basis throughout. Show usage coverage as attempts with complete usage divided by attempts expected to report, and pricing coverage as fully priced canonical calls divided by calls known to require pricing. List unresolved events and unsupported harnesses. Partial sums are lower bounds. Missing evidence never means free work.

Use a fixed delivery horizon per work class for policy comparisons. Freeze the cohort at first ready, include all attributable costs through that horizon, then allow 14 days for the last eligible delivery’s defect window to mature. A success is an accepted delivery within the horizon with no confirmed attributable defect during its observation window. Report unresolved quality evidence and late deliveries separately. Publish a provisional view while outcomes mature. With zero successful tasks the ratio is undefined and spend remains visible. Alongside it show first-pass success on the cheaper model, escalation rate, spend before escalation, substantive human corrections and p90 delivery time.

## Data and access

Start with the existing [usage contract](../../../crates/aq-observation-api/artifacts/usage.schema.json), [canonical usage projection](../../../crates/aq-accounting/src/projection.rs), [swarm contract](../../../crates/aq-dashboard-contract/src/swarm_contract.rs), [hold follow-ups](../../../crates/aq-server/migrations/0173_supervisor_hold_followups.sql) and [merge evidence](../../../crates/aq-server/migrations/0161_delivery_merge_evidence.sql). During implementation, audit source coverage for task-ready history, gate transitions, historic eligibility, parent-child usage links and confirmed defect links. Current-state reads alone cannot reconstruct elapsed intervals.

A server-side reader projects committed domain records in bounded batches. Key contributions by source identity and revision, checkpoint atomically with projected updates, and recompute affected intervals and cohorts on corrections. Preserve event time and observed time. Include definition version, cutoff, last processed watermark, sample size, units and coverage in reports. Backfill only time ranges supported by retained evidence. Record the earliest reliable date by measure. Following a restart, resume at the checkpoint and tolerate replay.

Expose read-only scorecard, task timeline, machine timeline and cohort-member queries through the dashboard’s existing authorization boundary, with project scoping, range limits and pagination. Keep task, attempt, session and PR identifiers in those queries. Export bounded labels to Prometheus for machine, slot phase, work class, approved model aliases and reason categories. Project-specific readers use scoped AQ data; fleet-wide Perses panels and the private scrape endpoint require fleet access. Export numeric observations and enumerated reasons only. No prompt or transcript bodies are needed.

Proposed starting defaults are 30-second fleet samples, a 24-hour machine view and a weekly cohort view. Retain 30 days of raw operational samples and 90 days of daily cohort aggregates initially, subject to existing source-retention policies. Display data gaps explicitly. Monitor reader lag, missed scrapes, series count and projection duration. Read cached aggregates on scrape. Eligibility reconstruction and dashboard queries must stay off the task admission path.

## Operator page and improvement goals

The top row shows on-time delivery, p90 ready-to-merge, first-pass acceptance, escaped defects and cost per successful delivery. Each tile links to its contributing tasks, pending outcomes and missing evidence. Below it, place the two-machine timeline above eligible demand and held-task count on a shared axis. Follow it with oldest actionable holds, stages ranked by task-hours lost, and cost by initial routing policy. Users can move from a time range to a machine or task and inspect the events underlying the result.

A goal stores its owner, metric definition version, scope, baseline and comparison windows, target, minimum sample and coverage, and quality guardrails. Show baseline, current value, delta, denominator and status. Insufficient coverage or an immature cohort produces “insufficient evidence.” Establish a baseline over at least one complete work cycle and enough mature tasks before adopting a numeric target. Compare similar work classes and show class mix when aggregating. For routing experiments, assign eligible tasks to policy variants at intake and retain that assignment through escalation.

For example, a fleet goal could reduce avoidable idle slot-hours per 100 online slot-hours while holding first-pass acceptance steady. A routing goal could lower estimated cost per successful standard task while maintaining delivery rate, p90 latency and escaped-defect rate. Operators choose thresholds after the baseline. Goal evaluation is read-only. Any subsequent routing change follows the existing owner controls.

## Delivery and acceptance

1. **Establish accounting parity.** Inventory producers and run Claude, Codex, delegated, unknown-usage, cumulative-reset and duplicate-stream fixtures. Replay the same evidence twice and verify identical totals. Verify that a rate-card correction updates the affected cohort once.
2. **Ship fleet and hold visibility.** Add bounded snapshot collection, private Prometheus export and the two-machine Perses view. Validate one real idle interval on each machine with scheduler evidence. Test one task eligible on two machines, two tasks sharing one remaining provider permit, an offline worker, a stale heartbeat and an owner pause. Verify that reminder traffic never resets the age of a real long-held task.
3. **Ship delivery and efficiency cohorts.** Reconcile a ready task through retries to its forge merge. Test a cancellation, a mechanical refresh, a multi-PR delivery, a late merge receipt and a linked revert on day 13. Trace a cheaper-model attempt followed by a delegated call and an escalation. The task should count once, cost should include all three contributions, and unknown spend should remain visible.
4. **Enable goals after baseline review.** Verify that every headline value can be reproduced from its authorized drill-down. Check a low-volume cohort, incomplete pricing, a missed scrape and a monitor restart. Disable monitoring during an execution smoke test and verify task progress. Measure admission latency and database load against the same workload with monitoring off before enabling collection fleet-wide.

Release the first dashboard when the two real machine intervals, long hold and escalated task reconcile to source evidence. Enable comparison goals after the first mature baseline. The proposal leaves numeric improvement targets and the production retention budget for owner review.
