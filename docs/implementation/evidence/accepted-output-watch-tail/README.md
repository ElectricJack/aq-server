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
