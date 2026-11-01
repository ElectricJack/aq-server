# Native read-only reconciliation

This slice implements the frozen0.1.7 Reconcile/ReconcilePage contract on the
machine listener. Current machine authentication is rechecked under the node lock;
assigned immutable bindings scope reads to the requested historical logical boot.
Retired launch configuration does not hide actual historical owner receipts. No
input journal revision/digest is adopted, no receipt is inserted, and no grant,
material lease, clock, claim or release is created or changed.

Explicit batches return one result per exact requested binding, up to32. Unknown,
foreign or mismatched bindings return only the caller-provided attempt ID and a
bounded refusal. Actual launch owners, when present, must match the requested
logical guardian/launch; a caller's last known grant can be absent after ACK loss
but cannot contradict an actual issued grant. Cursor mode scans at most32 owned
rows in UUID order per request, scoped to the original requested boot, with a
forward cursor even when an unsupported row cannot be represented. It does not
leak another node's assignments or unbounded retained artifacts.

The read transaction uses a consistent snapshot, a current credential/node lock
and bounded statement/lock/overall deadlines. It reads compact launch bindings,
hot state and actual owner receipt bodies. It never performs Graph, Git, private
file or provider I/O. Granted is a historical/current projection without any
DeliveryClock; expired, cancelled, revoked, unknown or incompatible-current-boot
active authority becomes StopRequired. Terminal Stopped and NeverGranted require
validated actual receipt provenance. A cleared claim without supported release
provenance refuses; no invented Released artifact is returned.

Final page time is sampled after reads, with temporal phases recomputed against
that time. All pages pass the frozen validator and transport byte bound before
returning. Missing/corrupt history remains conservative. The initial profile does
not implement a release producer or change frozen protocol/schema bytes.

Qualification will cover mounted HTTP authentication/body/nonce bounds and
human-listener separation; exact offered/preparing/granted/expired snapshots;
old-boot and retired-configuration history; actual Started/Stopped and NeverGranted
receipts; explicit scope/owner refusal and cursor progress; no state/journal or
receipt mutation; and credential rotation while waiting on the real node lock.
