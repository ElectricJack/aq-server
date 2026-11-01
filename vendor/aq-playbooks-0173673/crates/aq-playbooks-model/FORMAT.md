# Portable format 1

Artifacts use UTF-8 JSON, no whitespace or BOM, recursively ordered object keys
by Rust string ordering (Unicode scalar order), integer JSON numbers in signed
64-bit or unsigned 64-bit range, and serde_json's string escaping: quotes,
backslashes, standard control escapes, other U+0000–U+001F as lowercase `\u00xx`;
other Unicode is emitted directly. Fractional/exponential numeric values are
rejected; exact domain decimals use bounded strings. Canonical integer zero is `0`. Absent object fields and explicit null remain distinct.
Model fields serialize explicitly, including empty collections and Option nulls;
unknown model fields are rejected. Hashes are `sha256:` plus lowercase hex of
canonical bytes. The artifact digest hashes ArtifactContent; the executable
fingerprint excludes source binding, source mappings, compiler build and purpose.
Canonical bytes of the complete Artifact are the stored artifact blob.

Schemas form a closed bounded subset: null, boolean, bounded i64 integer,
bounded UTF-8 string (optional enum), bounded arrays, closed objects with explicit
required keys, and nullable. No refs, regex, floating point, open objects, dynamic
code, implicit unions or discriminator guessing, or implicit coercion. Schema depth is 32.
String length counts UTF-8 bytes. A schema is a contract, not an authority grant.

Expressions have no side effects and deterministic ordered arrays. References
traverse named roots using object keys or zero-based array-index strings. Missing,
null, and unknown are distinct; even boolean short circuits preserve unresolved
required inputs. Exists maps Missing to false and Known(null) to true; Unknown
stays Unknown. Filter preserves order; map preserves length/order; difference
preserves left order and rejects missing/duplicate identities in either input.
Collection lexical roots cannot shadow existing roots. Every evaluation has
explicit depth, cumulative node/element work and serialized-byte limits.

Source header syntax is an opening `---aq-playbook` line, a single JSON object
serialized as SourceHeader (may span lines), and a closing `---` line. Normalize
CRLF and bare CR to LF; preserve every other byte, including final newline.
Source digest hashes the complete normalized source bytes, including header.

A bounded explicit `union` schema admits values matching any of 2–16 alternatives.
Every alternative obeys the same recursive depth and payload bounds. Assignability
requires every source alternative to fit the target; it does not guess or coerce
values. This represents named business outcomes with different output shapes.

Join `member_keys` supplies stable declared participant identities. It is either
empty (legacy runtime-ID keys) or has exactly one unique bounded name per member.
A named result is a closed object keyed by those identities. Each value contains
`participant` (the actual child/operation identity), `outcome`, and `output`.
The compiler derives each participant's output from its statically declared
owned child/operation contract. Successful all/any-accepted joins narrow those
contracts to accepted business outcomes. All-success results require every key;
any/timeout results retain optional keys. Downstream ordinary typed references
can read, for example, `joined.review.output.subject`. Engine failure/cancellation
keeps nullable outcome/output outside successful qualification. Membership and
ownership remain runtime-checked; a declared result key is not bearer authority.

Callable dependencies declare `selection`: `exact` uses the captured artifact
hash; `catalog` resolves the named binding at root admission. A catalog allowlist
may constrain acceptable full artifact hashes; an empty allowlist permits the
current interface-compatible version. The captured artifact remains compilation
provenance. Compatible implementations accept every declared caller input and
produce only declared caller outcomes with assignable output schemas. Admission
also validates scope, current caller authority, and the entire selected acyclic
closure against one catalog revision, then freezes exact hashes for all children.
Changing activation cannot retarget an already admitted root or its grandchildren.
