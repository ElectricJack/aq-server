# Actual worker accepted-output qualification

This bounded qualification strengthens the existing independent static worker,
real daemon HTTP/SQL, real Git and observed tmux fixture. It contains no Codex,
model/provider call, fake accepted relay peer or new production route/schema.
The worker now performs a real outbound Watch poll after the actual admin Watch,
then publishes physical output. The server fixture compares typed UploadAck
exchange/peer/cursor/no-drop fields and exact accepted frame sequence/digest/bytes
with the authorized human read-tail. Output remains volatile resident data;
accepted publication does not imply durable terminal history or a Snapshot.

The selected case is explicitly historical: supported ordered state32 prefix,
then the fixture's existing state33 setup. It is not latest-format startup proof.
No new SQL migration or manual relabel is introduced.

## First attempt: fixture baseline failure

The focused all-feature native_initial_grant test target compiled successfully in
1m03s. The one authorized static case then failed during fixture setup in1.35s:
the genuine historical prefix was32, but the shared preparation helper asserted
current format35. It stopped before worker spawn, Watch or output publication.
The [exact negative record](historical-prefix-negative.log) is retained. No accepted
UploadAck, frame delivery or process-cleanup proof follows from this attempt.

The source-only correction makes that helper compare against the existing
`fixture_store_state_version()` selection:32 within the supported historical
scope and the current crate format otherwise (35 in this source snapshot). Store startup, schema guards, old fixture setup
and normal current-format expectations remain intact. Compilation and rerun of
the correction are pending the next explicit shared test lease.

Paired worker source `2dc9fd1` builds successfully in24.30s; its independent
smoke binary pin is `1dc17e7b25b8f572d4e242b97382df07248c454885f1073045b661fb8b1a376c`.
Server source candidate `ed0cf8c` changes only the existing process fixture and
output verifier. No runtime eligibility or production credential-isolation claim
is made.


## Second attempt: worker refusal before Watch

The historical-prefix correction compiled successfully in4.81s. The one additional
authorized static case ran13.29s, then received the independent worker's
`phase=refused` frame before `terminal_registered`. No admin Watch, node pull,
accepted upload or read-tail assertion was reached. The
[exact second negative record](worker-refusal-negative.log) is retained.

The existing Watch helper printed only the phase mismatch and discarded the
constant refusal stage. No exact worker cause or physical cleanup result is
inferred from that incomplete diagnostic. Source-only diagnostics now preserve
that constant stage through output-error cleanup, and print bounded stage/HTTP
status/selected history booleans before the assertion. They do not dump raw error
chains, credentials, material or arbitrary journal content. No timeout, lease,
DDL, provider route or actual Codex execution is changed. Compilation and another
actual static attempt require separate lease/authorization; none has run yet.


## Third attempt: diagnosed local source hydration refusal

The bounded diagnostics compiled on the existing target2 cache in1m46s. The
single authorized diagnostic case ran8.71s and refused at exact constant stage
`phase_hydrate`. All observed HTTP responses were200, including source metadata,
manifest and bundle downloads. Journal revision was18. No execution grant was
issued (`issued=[]`); execution deadline was absent, baseline/Running/Stopped and
both ACK flags were false, and ECHILD was absent. No terminal registration,
Watch pull, upload or authorized tail assertion was reached. The
[bounded log](hydration-diagnostic-negative.log) and
[structured diagnostic](hydration-diagnostic-negative.json) retain this failure.

Read-only source inspection found a strong explanation: the actual Git fixture
selects default artifact limits. Git import reserves its full10GiB expanded bound
plus256MiB free reserve before quarantine import. Required available space is
11005853696B; the run had7082278912B available, below that threshold. This is a
source-supported explanation, not a captured exact Git error code. The actual
storage-admission refusal must remain intact. No bound, timeout, lease or profile
was changed to obtain a passing test.

The diagnostic worker build used jobs2/debug0/incremental0 and passed in22.38s;
its copied binary SHA256 is
`adbf3230fc6c9d19de9c29a744e8f8ee0eccaa5aa6bacbcadb50c512c3638b20`.
The earlier worker and both previous negatives remain recorded separately.
An accepted-output proof still requires an independently sufficient environment
or an explicitly reviewed/provisioned stopped-node fixture profile, then a new
authorized attempt. No Codex or provider was invoked and no retry occurred.


## Fourth attempt: sufficient disk, earlier acceptance refusal

Root reclaimed only two inactive owned scratch Cargo caches; no source, repository
or pinned owner was changed. Before the next explicitly authorized single attempt,
all pins were rechecked and available disk was23643541504B, above the unchanged
default Git import requirement11005853696B. The existing diagnostic server binary
was reused without a Cargo rebuild; source since8c477d4 differed only in evidence.
The historical33 fixture, default Git profile and all timeouts remained unchanged.

The case ran4.31s and refused at `phase_accept`: register200/58ms, poll200/492ms,
proposal200/84ms and first accept409/152ms. Journal revision was3, no execution
grant was issued, execution deadline was absent, baseline/Running/Stopped and both
ACKs were false, and ECHILD was absent. It never reached source hydration or any
terminal Watch/pull/upload/tail step. The exact application reason for409 was not
emitted; no inference that it was expiry or a changed authority fact is made.
The [bounded log](headroom-acceptance-negative.log) and
[full pin/status observation](headroom-acceptance-negative.json) retain this result.

This distinct earlier refusal cannot verify or disprove the prior source-supported
disk diagnosis. No timer, profile or admission check was weakened, no retry followed,
and no Codex/provider call, host execution, accepted output or production claim
follows from this attempt. The shared build target was released after completion.


## Source-only acceptance classification and original clocks

The real control API deliberately erases internal errors into public generic
failure codes, so reading an HTTP409 body cannot recover its branch. A narrow
`test-support` response extension now exposes only a fixed `ConflictClass` enum
inside the router; it adds no header/body field and does not change production
builds. Exact known literals map to fixed categories; dynamic/unknown conflicts
become `other_conflict`. No error string, credential or body is retained. Regression
sources check unknown-message redaction and byte-identical public failure output.

The fixture also records the original proposal publication/offer clocks,
selection budget/node/project/source expiries, Graph read stamps, and local
reservation/phase-accept intent/deadline fields. Selected SQL clock fields are
reduced to unsigned integers or null before logging. No sealed timestamp or
observation is rewritten. The checks below compiled this increment under a
separate explicit shared build lease; no further process case has run.

Source inspection distinguishes the clocks: production already re-reads actual
Graph/context, resolves selection again and obtains a real configured-source
observation after external Git export. Source observations last30s. The retained
selection budget has a separate2000ms freshness bound that first acceptance
repeatedly fences. A30s preparation lease starts only after accepted publication.
Therefore the earlier long preparation run does not itself establish why first
acceptance refused. The new fixed classification and original clocks must identify
that cause before any fixture boundary, build profile or performance change is
proposed. No source renewal, fabricated observation, TTL or timeout relaxation
is introduced by these diagnostics.

### Diagnostic source checks

For source `4fc2360`, both focused pure library cases passed: the arbitrary-message
fallback and the byte-identical redacted wire failure (one test each, 0.00s).
The first library artifact rebuild took 1m14s; the two qualified test invocations
reused it. An initial short-name exact filter selected zero tests and is not
counted as verification. The actual integration test was compiled with
`--features test-support --no-run` in 4.79s, without invoking its database,
worker, host or any model. All Cargo commands used the existing target2 cache,
jobs2, debug0 and incremental0; that lease was released afterward.

The resulting test artifact is
`native_initial_grant-6d7239cfc04f9e89`, 98,844,432 bytes, SHA256
`d0d871db9cebe7e9e2127a5a0dc9f8efccb644c967b07d5b22e40225cfefdbbc`.
This compile evidence does not identify the earlier HTTP409 cause and does not
establish accepted output. A separately authorized diagnostic process attempt
would be needed to obtain the fixed refusal category and original clocks.

## Fifth attempt: successful acceptance, historical-store execution guard

The single authorized classified diagnostic reused the pinned `4fc2360` artifact
above and all unchanged worker/source/host pins. Available disk was22,717,546,496B.
The case ran11.53s. First acceptance200/225ms and its replay200/106ms had no
conflict class. First acceptance completed950ms before the original budget/offer
expiry, while the real source observation retained its separate30s bound. The
earlier409 was not reproduced and remains unexplained.

All artifact/material/source reads succeeded, including local hydration. The
worker then refused at `phase_execution_http`, with Prepare503/4ms. Journal
revision26 had no issued execution grant, execution deadline, baseline, Running,
Stopped, ECHILD or either ACK. No terminal/Watch/pull/upload/tail step was reached.
The [bounded log](classified-prepare-negative.log) and
[structured pins/clocks/history](classified-prepare-negative.json) preserve this
distinct outcome. No matching pinned child process remained in the post-case
process observation; this is not a substitute for subtree ECHILD proof.

Read-only source inspection identified a concrete fixture assembly bug: the real
Prepare transport requires meta version equal to its compiled
`STATE_FORMAT_VERSION` as well as the actual migration33 terminal table/trigger.
This frozen binary expects35; the historical fixture genuinely reaches33, so the
readiness condition is false before the issuer. A separate ordinary current-store
entrypoint must use actual Store startup and already-applied migrations. The
runtime guard, historical32→33 helper behavior, default timers and Git profile
remain unchanged. This failure supplies no accepted-output or production proof.

## Sixth attempt: genuine current Store, fresh Prepare conflict

Source `b0020c7` adds a separate `current_store_...` entrypoint using ordinary
Store startup and recognizes the exact compiled current version in the fixture
helper, preserving the historical32→33 path. It changes no runtime guard or
database metadata. Its focused no-run build passed5.01s; test binary SHA256 was
`8e56447edcaa437917c0ffcbf5981c693d1916888d262cd50c14d06ef5b44e26`.

The single changed static case had21,890,113,536B available and ran10.37s.
Both accepts and all source hydration reads returned200. The storage503 was
resolved: execution Prepare entered the current owner and returned409/399ms.
The exact Prepare conflict category was not yet captured, because the fixture
middleware observed that extension only on acceptance. Journal revision26 again
had no issued execution, execution deadline, baseline, Running, Stopped, ECHILD
or ACK. No matching pinned child process remained afterward, with no stop-proof
inference. See the [bounded log](current-store-prepare-negative.log) and
[structured evidence](current-store-prepare-negative.json).

The fresh phase owner reads current project/Graph/source/private-file and locked
inventory/principal/availability facts for the accepted exact choice. Source
inspection does not show a call to the original selection's2s clock fence in
that owner. It would therefore be incorrect to attribute this new409 to the
expired admission budget without observing the actual branch. The next bounded
instrumentation observes the existing fixed extension for Prepare as well and
classifies its two exact static conflict literals. No unchanged retry, source
restamp, TTL extension, guard weakening or provider call follows from this run.

## Seventh attempt: exact stale synthetic-health refusal

Source `3fe4fd5` observes the fixed Prepare extension, and `2c03b79` identifies
the actual current-health refusal branch only in test-support builds. The public
wire remains identical, verified for both selection expiry and health refusal;
the unknown-message fallback also passed. The library rebuild took1m15s and the
fixture no-run build4.72s. Test binary SHA256 was
`0b8899846d046d4e7cc937821462109cbb9a80767974325d11371fd6b1542efc`.

The single newly instrumented case ran10.49s with21,368,180,736B available.
Prepare409/388ms carried exact class `phase_execution_health_unavailable`.
The existing synthetic operator assessment sequence2 was6,691ms old when Prepare
arrived, beyond the unchanged pinned5,000ms maximum age despite its30s TTL.
The actual health owner correctly refused. Both accepts and hydration reads
succeeded; journal26 still had no execution grant, host, terminal or ACK.
The [bounded log](current-health-classified-negative.log) and
[structured evidence](current-health-classified-negative.json) retain this proof.
No matching pinned child process remained in the post-case process observation.

This identifies a fixture observation boundary, not a stale original selection
or a reason to increase the health bound. A corrected static fixture must obtain
a new explicit synthetic operator assessment after genuinely revalidating its
stated deterministic-peer/material/harness/node/journal facts at PreparedInputs.
It must retain sequence2 and the unchanged policy, and make no claim about actual
model/provider availability. No unchanged retry or automatic runtime refresh is
authorized by this evidence.
