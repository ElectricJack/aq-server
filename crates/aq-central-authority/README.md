# aq-central-authority

Pure taskless central-session data contracts and lifecycle reducer. This crate
never authenticates callers, grants execution authority, accesses secrets, starts
processes or writes messages. Runtime capabilities are always false. A future
owner must authenticate source references and persist/fence transitions before
using them. Model phases are reported historical data, never process freshness.

See ../../docs/implementation/AQ-17-central-authority-core.md. Version 1 wire data
is bounded to 64 KiB; artifact refs and SessionIdentity come from the pinned public
session protocol. Central session/outer generations must match; Remote identities
and project/task/node fields are unsupported. One plan contains one immutable
mail batch and one bounded turn; no interactive injection/resume is implied.

Initial model construction is generation 1 only. Successor construction requires
bounded all-generation mail/cursor and cross-role operation history; authenticated
source ownership and durable fencing remain the future owner's responsibility.
The schema records shapes and local bounds; always use `decode` for cross-field
validation. Even validated restored state or an Applied transition is not a grant.
