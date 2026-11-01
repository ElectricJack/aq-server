# Explicit empty configured Playbooks owner

Source `2baa8e7b387b52717ccc9b97a6b970b447cf319b`, based on composed AQ37
`1111441`, passed focused qualification. Root reviewed the production change:
configuration accepts an explicit `deployments=[]`; runtime skips the historical
startup-definition capture only when that configured array is empty. All namespace,
project, owner, authorization, private-file/key and real-socket requirements remain.
Omitting deployments is still invalid; its maximum remains four.

Current catalog support comes from the actual attached owner query interface,
independently of startup capture. A fresh empty owner returns successful current
Definitions/Activations pages with zero items. The unchanged historical loaded
profile requires1..4items and therefore stays unmounted/unavailable in this case;
its DTO/schema/parser/capability code was not widened. No dummy policy, artifact,
grant, recipient, expiry or activation is required to make inspection available.

`[]` describes startup configuration, not an uninstall or an assertion that the
persistent catalog is empty. The retained-namespace test installs a real definition
and binding, shuts down, removes the configured artifact file, and starts the same
owner with `[]`. The exact stored current definitions/activation and grant remain
unchanged while historical startup capture remains unavailable. No reader is
relabelled as current from an old captured configuration.

Actual PostgreSQL tests passed:

- Five current-catalog cases in1.89s, including fresh empty namespace startup,
  zero grants/artifacts/bindings/catalog rows, two idle quanta, actual current caps,
  stop/restart incarnation advance and post-stop capability refusal; plus retained
  populated namespace with `[]`, and the existing scope/role/query/principal fences.
- Three unchanged startup/redaction/inert-owner regressions in1.19s.
- Existing disabled/TCP/private-file/unknown-field/socket-directory guard case
  in0.64s.

All17 work/effect counts are compared to the actual fixture baseline. That baseline
contains existing fixture execution state: this is preservation, not a claim that
all test data is zero. Runtime metadata, leases and migrations are real writes.
The fixture transport remains explicitly synthetic local UDS forwarding. The
separate real-UDS current-owner proof on AQ37/component0.1.3 is retained in
[the actual UDS record](../private-uds-current-catalog-37/README.md).

Affected server lib/test strict lint with all features passed34.34s; owned rustfmt
passed. One default-feature locked/offline binary build passed61s, with no
test-support feature. Its0500 pinned copy is
`/tmp/aq-empty-owner-qualified-2baa8e7/aq-server`, SHA256
`6f07dd8e98d3ae74e4886a6bf66b10b8f85a0a22c05756b77738543c7ec7da65`.
Runtime source stamp is the unique `2baa8e7`; the pin retains the full commit.
No service was started by that build. The shared build target was explicitly
released before database stage preparation. Logs, pin, JSON and checksums accompany
this record. This does not by itself enable or qualify a canonical owner.
