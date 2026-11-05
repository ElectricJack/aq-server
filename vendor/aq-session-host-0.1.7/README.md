# AQ session host 0.1.7

This independent Linux library and helper executable host one immutable agent
session in a private tmux server. They depend on `aq-session-protocol =0.1.0`,
not the worker scheduler, journal, node protocol, daemon, database or provider.
Central sessions use their own `SessionIdentity::Central`; no worker identity is
fabricated. The worker shares only process-ownership primitives with this crate.

The qualified mode is tmux **3.4**, `-D -u -S <private socket> -f /dev/null`.
The [upstream manual](https://man.openbsd.org/tmux.1) documents `-D` as foreground
server mode and `-N` as refusing server startup. Tests establish the actual server
PID and ancestry on the tested installation. Every client uses `-N`, and pane
creation executes the helper with multiple explicit arguments. The helper then
executes the adapter's native argv directly; no task text is shell-interpolated.
No shared server, arbitrary network tmux command, implicit shell, live process
reattachment, transport relay or real-provider adapter is implemented.

## Embedding contract

Implement `HarnessAdapter` and `LaunchAuthority`, provide a canonical workspace,
exact installed tmux version/helper paths, credential-profile availability and
startup context material. `prepare` must refuse unsupported model/settings,
required scope/context delivery and native limits; it returns exact manifest,
adapter, CLI and environment identities plus advertised capabilities. The host
checks these bindings, local material sizes/digests and protocol bounds before
recording intent. Adapters are trusted installed code, never arbitrary downloaded
plugins. Credential references are availability declarations, not secret values;
this increment neither provisions secrets nor qualifies a provider credential
handoff. The default child environment excludes caller/worker credentials.

`LaunchAuthority` owns admission, current permission ceilings and durable state.
Each callback returns only after its caller-owned transaction is durable:

1. `record_launch_intent` precedes spawning the dedicated helper.
2. `record_guardian` persists its exact nonce/process/boot/session identity.
3. `record_start_intent` precedes the bound start request and all tmux effects.
4. `record_started` persists the observed server/pane identities.

The helper waits for that exact start request and independently checks its
expiring `ExecutionPermit`. It records Starting before spawning tmux and checks
permit, cancellation and wall deadline before server/pane creation and immediately
before native exec. A failed callback never authorizes the next effect. A caller
crash may leave a Waiting/Starting process or writers; retain the intent and
resources and reconcile them. `SessionHost::recover` requires the caller's exact
immutable `LaunchIntent` and only stops/preserves; it cannot launch or recreate a
missing server/pane. Repeating a launch ID refuses an existing directory.

`HostPhase::Running` means the OS session/pane has started. It does **not** mean a
model initialized successfully. `HarnessAdapter::readiness` supplies a separately
qualified native observation. The synthetic adapter tests prove no real harness
or model readiness, permission semantics or source-result behavior.

## Resource policy is explicit

The host enforces local monotonic permit expiry, the immutable attempt wall limit
and bounded termination grace. It **does not enforce** manifest memory, CPU or
process-count ceilings. Those ceilings require admission/reservation and actual
environment enforcement by the embedding application and trusted adapter.
`LocalEnvironment.resource_policy` forces an explicit choice:

- `CallerAdmission(reservation)` records caller-owned capacity admission only.
- `RequireQualifiedEnvironment(qualification)` refuses unless the installed
  adapter returns that exact immutable environment qualification reference.

The adapter must verify that the installed enforcement configuration covers the
requested manifest limits; a matching artifact name or positive integer alone
never establishes enforcement. The chosen policy/evidence is frozen into the
local configuration digest. Only synthetic **CallerAdmission** mode is qualified
here. Hard memory/CPU/process enforcement, cgroups and hostile-process isolation
are not claimed. AQ-17/WRK-08 must refuse required ceilings they cannot establish.

## Ownership, control and bounds

A dedicated, initially childless, single-threaded Linux subreaper owns the
foreground tmux server, pane and adopted/double-forked descendants. Start verifies
the server PID is the spawned child and the pane remains in its ancestry. Retained
pane pidfds detect exit without spawning polling processes; steady-state hosting
spawns **zero** tmux inspection clients. Explicit `capture_text` is a bounded,
development text view, not a terminal renderer snapshot. It cannot create a server
or pane and refuses after stop; retained output/rendering belongs to WRK-14.
Client disconnect/detach does not stop or relaunch the agent.

Stop/expiry have priority over renewals and new effects. Termination signals owned
children repeatedly, including adopted writers, then escalates to SIGKILL. Only
`waitpid(__WALL) == ECHILD` establishes the exact guardian's stopped-writer proof;
PID disappearance, pane exit, terminal EOF and empty procfs scans do not.
`StoppedProof` is a distinct opaque host type, not legacy worker proof. Guardian
loss remains StopUncertain even after known PIDs disappear. A stopped receipt is
re-read after guardian exit to avoid confusing a final-receipt race with loss.
Ordinary startup/renewal validation errors enter Stopping and reap; an I/O/kernel
failure may prevent receipt publication, in which case cleanup is best effort and
stop remains uncertain. No source checkpoint/resource release may infer proof.

Renewal compare-and-publication is serialized by an owner-only, nonblocking file
lock. A durable bounded high-watermark stores the exact latest permit before
publishing it. The exact latest retry is a no-op, changed bytes under that sequence
refuse, and older sequences are stale. A crash between intent and publication can
republish the exact latest permit on retry. Renewals cannot revive expired,
cancelled or wall-limited execution. Lock contention explicitly refuses for retry;
there is no hidden blocking lock wait or unbounded renewal-history growth.

Local files must be regular files and are opened nonblocking without following
symlinks. JSON state/config files cap at 2 MiB; startup context material also obeys
its manifest's aggregate byte/token bounds. Private host and launch directories
must be owned by the effective user with mode 0700; new files are 0600. Local
commands cap at 128 arguments/32 environment entries, with individual bounds and
protected environment keys. Each tmux command drains nonblocking output up to
64 KiB under one absolute 250 ms deadline, including after child exit. Timeout
cleanup never blocks waiting for an uninterruptible child. The guardian retains
such command children for its kernel reaper; caller processes transfer them to
the bounded parent registry.

The parent lazily creates **one** 128 KiB-stack reaper thread with at most 1024
reserved/tracked local children (helpers plus temporary client commands). Capacity
is reserved before spawn; exhaustion refuses new launches/commands. All helpers
transfer to that registry immediately, including callback/error paths. The reaper
only calls nonblocking `try_wait`; it never stops or renews execution and never
asserts writer-stop proof. Dropping a handle does not cancel its session. A bounded
post-proof wait observes helper exit, while the registry reaps it independently.
Guardian reaping/signaling uses batches of at most 256 children/syscalls per tick;
only an actual ECHILD result marks completion. Sustained adopted-child exits cannot
create an unbounded vector or monopolize expiry/cancellation checks.

## Validation and remaining integration

Routine tests need no tmux, daemon, database, sibling checkout or provider:

```
cargo test -p aq-session-host --lib
```

The explicit Linux/tmux 3.4 qualification suite is separate:

```
cargo test -p aq-session-host --features test-support --test local_tmux -- --ignored --nocapture
```

It covers central/remote identities, interactive literal text, private socket/pane
collisions, failed durable start, six caller-crash boundaries, four guardian-crash
boundaries around server/socket/pane creation, escaped writers, sustained adopted
child exits, cancellation, independent expiry after caller death, malformed and
concurrent renewal fencing, wall limits and no fallback/relaunch. Test-only fixture
and SIGSTOP fault hooks exist only with `test-support`. The fault-resume test waits
for actual kernel T state rather than racing a marker against SIGSTOP.

One local idle synthetic sample measured RSS of approximately guardian 3956 KiB,
tmux 5832 KiB and pane 3556 KiB. These vary by build/workload; they are resource-cost
observations, not deployment concurrency, isolation or latency qualifications.

`aq-worker` 0.3.2 consumes shared process evidence, pidfd signaling and BOOTTIME
primitives, preserving the existing guardian wire/journal behavior. It explicitly
continues to refuse profiled/tmux scheduling. WRK-08 must add manifest-bound worker
admission, source preparation, durable host evidence/migration, recovery and typed
stop-proof gates for checkpoint/report/resource release. AQ-17 must implement the
central expiring-authority adapter. Real harnesses, provider credentials, native
permission qualification, terminal input transport and full TUI snapshots remain
separate work. No new product binary pin or public publication is implied.

Version0.1.1 adds experimental `terminal_io` for an explicitly bound existing
host. Public launch/guardian/observation and Started callback shapes stay unchanged.
Guardian-owned literal input/resize use AEAD mailboxes, durable no-replay receipts,
controller deadlines, retained directory identity and guardian revision highwater.
`observe_existing` verifies an exact physical host without starting/resuming or
renewing it; legacy `recover` remains stop-and-preserve. This does not qualify a
renderer, snapshot barrier or worker scheduler adoption. See WRK-14 stage1.

Candidate0.1.2 adds a bounded read-only control-mode output observer and a durable
one-shot claim. It preserves literal bytes and reports pause/exit/loss; it cannot
produce renderer snapshots or recreate a lost producer. Its reader client is
owned by the existing guardian; losing that reader cannot establish stopped-writer
proof by itself. Encrypted
IO metadata adds a required output-claim field; older IO state is refused, not
implicitly rewritten. Stop-and-preserve remains available; this is not qualified
hot upgrade of a running0.1.1 terminal. Launch, GuardianIdentity, HostObservation
and StoppedProof serialization are unchanged.


### Retained negative experiment

The initial source14354e7 (host0.1.2/bridge0.1.1) is frozen separately with
negative evidence f05bea5. Actual tmux3.4 tests reproduced a stopped sole observer
blocking the harness in a PTY write for more than5seconds despite
`pause-after=1`. Guarded execution expiry still stops the writers, but continued
harness progress is not qualified. Passing individual runs do not remove this
failure. The control-only checkpoint remains distinct; no output capability is
advertised. The current experiment replaces worker-owned spawning with guardian-owned
attachment and a1second independent read lease. Only a timestamped EAGAIN drain
may renew; old queued receipts cannot reset its lifetime. A stopped child,
missing/slow consumer, lost ready/ACK or closed local channel severs the exact
read-only client, never the pane/server. Guardian waitpid conservatively includes
the reader in stop evidence. A private bounded local SOCK_SEQPACKET channel
passes one stdout FD, with one request/process check per tick (5ms sleep plus
work/scheduling). Quiet drained renewals are bounded to four per second. This
adds one local listener per host, one native reader plus pipes/pidfd/channel while
attached, and fixed8KiB socket buffer requests (kernel overhead is additional).
The strict decoder API retains its unchanged event bound. Streaming observation
now returns at most64events per poll and retains at most one64KiB raw packet
suffix; it parses that suffix before another read and cannot renew until it is
exhausted and the actual pipe reaches EAGAIN. Absolute256KiB line and64KiB decoded
event bounds remain fail-closed. Full renderer/baseline and producer adoption
remain unqualified.


## Explicit observed-launch component

`observed::ObservedStartup` combines the unchanged `RenderedStartup` literal
payload, exact physical terminal identity, installed adapter/renderer references,
fixed geometry and the **original** CLOCK_BOOTTIME hard deadline. The separate
`SessionHost::prepare_observed` path returns a closed `PendingObservedSession`;
it remains Starting and cannot return an active terminal controller. Legacy
`launch`/`launch_rendered` keep their behavior and omit the optional private
observed configuration field. HostObservation/GuardianIdentity/LaunchIntent and
StoppedProof wire shapes are unchanged.

`SealedElfPool` admits at most four images/16MiB per explicit process-local pool,
with a4MiB per-image bound and64KiB copy/hash scratch. It copies verified ELF
bytes into an executable memfd, verifies kernel immutable seals, and retains the
same immutable object for reuse; a fresh path hash alone is not immutability.
There is no implicit eviction or budget reset. The embedding must reserve
aggregate pools, retained old owners and process resources across restart; these
are logical component quotas, not RSS/cgroup enforcement. Executable memfd/seals
are required and unavailable kernels refuse. The first actual fixture is a small
statically linked synthetic ELF. Sealing an ELF does not pin a dynamic loader,
libraries, tools or model executor; installed adapter/environment qualification
must cover those dependencies and the actual helper/tmux installation.

The guardian owns the exact sealed descriptor before any pane starts. The fixed
helper emits no terminal bytes while closed and reports its original PID/start
identity and TIOCGWINSZ geometry over owner-only bounded local packets. Before
returning `VerifiedBlankBaseline`, the guardian checks the exact acknowledged
one-shot reader, original-time EAGAIN drain, live/non-stopped reader and helper,
current geometry and immutable launch binding. The caller must also observe the
attach/drain with no pane output. This proof is opaque/Serialize-only; it is a
narrow blank premise, **not a renderer Snapshot or an output/resize barrier**.
The first profile uses a fresh tmux3.4 server, status off, fixed pane dimensions,
and a read-only ignore-size observer. No capture_text state is substituted.

`release_after_baseline` requires durable caller-owned baseline acceptance via
`ObservedLaunchAuthority::record_baseline`. The guardian persists one exec intent,
rechecks authority/reader/geometry after persistence and hands off the held FD.
The helper revalidates exact payload bytes, seals, cancellation, guardian liveness
and the original permit/hard deadline, then uses execveat with no path/shell
fallback. Running requires CLOEXEC peer closure **and** the same live original
PID/startticks with `/proc/PID/exe` matching the sealed device/inode. EOF alone,
exec failure, missed short-lived execution and failed durability remain post-grant
uncertainty without Started; they never imply NeverGranted or safe release.
`poll_started` is nonblocking so callers can drain output between observations.
No baseline readiness step renews execution authority.

This candidate is not adopted by the worker runtime/native producer, and proves
no provider readiness, Codex compatibility, complete tool ownership, renderer
codec/browser agreement, physical Unicode/alternate-screen compatibility, resize
sequencer, snapshot barrier, reconnect resynchronization or unknown-input
resolution. Preexec observer loss is one-shot and refuses release; after exec,
reader loss retains the existing bounded detach/discontinuity behavior, requiring
a future qualified resynchronization protocol rather than implicit reattach.


## 0.1.7 atomic request publication

New durable files, including `start.request` and `stop.request`, are fully
written to a unique mode-0600 temporary file in the destination directory and
fsynced before a Linux `renameat2(RENAME_NOREPLACE)` publishes the final name.
The containing directory is then fsynced. Existing requests, including symlinks,
are never replaced. An unavailable no-replace rename fails closed. A guardian
still stops on genuinely malformed published JSON; it does not retry parsing or
ignore invalid input. There is no wire or admission-policy change.

Use the matching 0.1.7 library and helper from the reproducible source release.
See [F1a consumer pin and admission guidance](../../docs/implementation/F1a-host.md).
