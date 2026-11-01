# Dashboard qualification status — November 1, 2026

This records bounded source and isolated-runtime qualification at agent-q
`b2d7600` and dashboard `436fc12`. It updates the execution plan without closing
the full product/design tasks. A compiled bundle served by an isolated test
listener is positive integration evidence; it is separate from promotion to the
canonical local preview. No canonical preview promotion is asserted here.

| Task | Qualified increment and evidence | Remaining completion gate |
| --- | --- | --- |
| API-04 / UI-07 | Current-Owner review discovery, retained acceptance/result evidence and exact-revision answers. [Actual protected-owner browser reads](API-04-browser/README.md) passed in 2.75s. [Disposable browser actions](API-04-browser/actions.md) passed in 2.88s: approve/reject double clicks each sent once, a committed answer with truncated HTTP response remained unknown, and an explicit identical-byte retry recovered its original receipt. | Full review evidence/design scope remains open, including document/media presentation, discussion and changes-since comparisons. Actual actions did not merge, deploy or complete a task. Preview promotion requires a separately recorded mounted review owner and served asset identity. |
| API-05 / UI-08 | [Configured run/admission inspection](API-05-playbooks-inspection.md), exact artifact/dependency pins, receipts, operations and child links. The [actual parent/child compiled-browser proof](../../../aq-dashboard/docs/evidence/configured-playbooks-browser/README.md) passed in 4.08s (1.237s browser): typed owner installation/activation/admission, a blocking child, four parent receipts, navigation/reload and 390px layout. | Definitions/activation discovery and supported owner actions are deferred. The receipt timeline is not a complete step graph. Full mockup acceptance remains open; a disabled optional runtime is unavailable, not an empty successful list. |
| API-10 / UI-02 | [Typed task-create contract](../../protocol/human-task-create/v1/README.md), exact retained request/Graph bytes, operation lookup and explicit recovery. The two-test owner target, including independently running Graph + SQL response-loss qualification, passed in 1.34s at source `265c3d0` (integrated as `68bd517` / `f328194`); five validator tests and 12 existing agent mutation regressions passed. State34 upgrade/history qualification passed seven tests in 1.81s at `62bf8d8`. The [Work composer](../../../aq-dashboard/docs/implementation/create-task-live.md) passed 44 focused UI/Graph regressions and typecheck, then actual compiled-browser creation in 861ms: exactly one task POST, matching request digest/retained receipt/current task, and reload lookup without another POST. Root reviewed desktop/390px composer and receipt captures; the isolated state34 database retained one confirmed intent and zero execution attempts. | Task editing and knowledge author/correct/verify/retire remain unimplemented parts of API-10. Response-loss/recovery has actual owner/API and synthetic UI coverage; the successful browser creation/reload path does not independently qualify browser fault recovery. Optional 320px long-project Scope-selector overflow is a separate open shared-shell issue. A draft/active task is not execution authority. |
| UI-12 | [Current-account notification inspection](../../../aq-dashboard/docs/implementation/notification-inspection.md) has 20 focused adapter/component tests and typecheck. Actual mounted router/SQL fixture `dashboard_notifications::mounted_notifications_read_current_principal_self_without_external_delivery` passed in 0.80s at `1cab587`, including Owner/Viewer self-only reads, foreign-recipient refusal and mounted-capability checks. | Positive compiled-browser inspection and reviewed desktop/phone captures remain pending. This fixture has no external destination, delivery or route. It does not establish external Discord delivery, acknowledgment, escalation/digest behavior, or the rest of UI-12's worker/profile/session panels. |

The Decisions and Playbooks browser proofs used the unchanged 28-asset bundle
with manifest SHA256
`85fa3dbfb790467dcaca3be6ce6ee7f448d9b2b39603f066df3b3811afbde2a8`.
They used disposable, labelled persisted fixtures, actual HTTP and domain owners,
and no browser response interception or provider calls. Their fixture listeners
shut down after qualification; they do not prove that optional owners are running
in the canonical preview. The Playbooks fixture's fixed 1970 timestamps are test
data, not current activity.

The subsequent task-creation proof used composite bundle manifest SHA256
`47cb250813812f3ee5c4afa0de426926a917e3918887c499f391efc6bbd1952f`.
Root also restored the canonical state33 backup into a disposable staging
database, upgraded that copy to state34 and booted its actual Graph/AQ listener
on port 4204. The staged Work/Knowledge Graph browser check passed in 3.65s with
16 reads and zero mutations against the same bundle. This is a staged upgrade
and regression check; the canonical port 4188 process/database had not changed
when this evidence was reported. Final promotion remains a separate record.

API-04, API-05, API-10, UI-02, UI-07, UI-08 and UI-12 remain `in_progress`.
AQ-24's earlier completed transport qualification is unchanged; a read-only
Settings panel does not requalify real external delivery. UI-01/UI-03/UI-04,
REL-03 and the full integration/product gates remain open.

The schema34 qualification used new disposable PostgreSQL databases and the
actual migration prefix. It preserved legacy execution/source rows across
33→34, proved an actual 33 reader refuses 34 without changing rows, and rejected
missing/disabled history guards without repair. Immutable identity/status,
terminal records, decreasing dispatch count, deletion and truncation checks
passed. It did not upgrade the canonical preview database. Rollback across a
state-format upgrade requires the matching pre-upgrade backup and binary.

Source validation is scoped: API-10 strict library/target lint passed in 23.60s;
schema34 scoped library lint passed in 25.96s; notification affected library/test
all-feature lint passed in 29.76s. These are not a full-workspace rerun or fleet
performance benchmark. No new build or runtime qualification was performed to
write this status record.
