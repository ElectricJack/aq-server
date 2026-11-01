# Native daemon/worker Prepare assembly probe

This opt-in diagnostic probe joins independently built components over their actual
interfaces. An explicitly pinned test-support worker executable connects outbound
to a real worker HTTP listener backed by PostgreSQL. Separate actual aq-git
processes own central publication and worker import/workspace state. No repository
path dependency, shared Rust type instance or direct worker database access joins
the two processes.

The worker creates its own new durable journal/boot, registers, polls, reserves,
accepts, downloads every original inline artifact and source range, installs the
private material, hydrates the worktree, records the exact Prepare intent and
requests initial authority. It keeps the actual PreparedInputs owners through
the response. Its success assertions require actual checked-out bytes and retained central
material receipt equality, a granted hot state, held task claim and absent Started.
The latest optimized batch run passed these assertions in3.10seconds overall.
An earlier run of the same pinned worker exhausted its preparation deadline
before usable grant delivery; this tiny-fixture success is load-sensitive.
The test terminates without invoking a host or publishing an execution result.

The fixture deliberately supplies synthetic runtime/harness qualification,
provider material and Graph responses. A bounded stdin handshake allows the daemon
fixture to record a new synthetic inventory observation for the worker's actual
new boot before any offer exists. This is not a qualification of inventory
reporting or a real model/harness. Existing sealed timestamps and permits are not
retimed. The independent worker binary's digest is required and checked before
execution; credentials travel only on its stdin, never argv or retained evidence.

The worker test binary computes the configured Git reservation before polling
and reserves that exact logical amount. The fixture's 64GiB logical capacity is
not measured physical capacity. No production scheduler, capacity advertisement,
restart recovery, renewal, closure/settlement or default runtime activation is
claimed. Those remain separate product gates.

Run only with explicit `DATABASE_URL`, `AQ_TEST_GIT_CANDIDATE`,
`AQ_TEST_GIT_EXECUTABLE`, `AQ_TEST_NATIVE_WORKER` and
`AQ_TEST_NATIVE_WORKER_SHA256`. The ordinary suite ignores this process/binary
qualification; its retained run must name the exact worker and Git source/digests.


## Measured limitation and narrow assembled success

Frozen selection and budget observations live for 2 seconds. Poll/Accept and
preparation consume that same interval; the last optimized run delivered an
original preparation plan with 1,264 ms remaining. Batching removes avoidable
journal writes but does not create a new authority phase. In that run all 36
original inputs, actual private installation, central source download, worker
Git import and workspace hydration completed. Prepare returned HTTP200 in204ms,
but the worker refused the initial delivery after its conservative local cap.

A server200 is not proof of live permission at the worker. Equally, refusal or
response loss is not proof of NeverGranted: the immutable server issuance and
held task/source state must survive for reconciliation and qualified launch
closure. On failure the probe records bounded grant IDs/bindings/versions,
original clocks, hot status and task hold, without credential or material bytes.
It never launches a host, changes deadlines, releases claims or retries with a
new launch identity to turn a refusal into success.

The ignored probe intentionally retains its live-grant assertion and can fail
under the current profile, as the retained negative shows. Its latest passing
result qualifies only the actual tiny-fixture interface path at the named pins.
It is not cold-workspace or load qualification, an ordinary suite or a benchmark. A separately negotiated accepted
preparation phase with strict fresh initial confirmation is required before
claiming practical cold preparation. See the
[phase proposal](../../../aq-worker/docs/implementation/WRK-08-preparation-phase-separation.md).
Earlier debug/unbatched release failures remain retained alongside the optimized
batch result; none is replaced by a later run.
