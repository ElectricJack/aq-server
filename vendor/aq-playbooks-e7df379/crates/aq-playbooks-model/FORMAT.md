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
stays Unknown. Filter preserves order; map preserves length/order; an item whose
body is Unknown makes either one Unknown at once, and an item whose body is
Missing makes it Missing only once every item is evaluated and none was
Unknown, so a Missing item never hides an Unknown one; difference
preserves left order and rejects missing/duplicate identities in either input.
Collection lexical roots cannot shadow existing roots. Every evaluation has
explicit depth, cumulative node/element work and serialized-byte limits.

`lookup` chooses a value by a key learned at run time, from a literal table,
with a required default:

    {"op":"lookup","key":{"op":"ref","root":"input","path":["kind"]},
     "cases":[{"when":"chore","then":{"op":"literal","value":"simple-high"}},
              {"when":"design","then":{"op":"literal","value":"complex-high"}}],
     "default":{"op":"literal","value":"standard-high"}}

The key is evaluated first. An Unknown key is the result, unchanged; it never
falls through to the default. A Known key selects the case whose `when` equals
it (JSON value equality, which for canonical values is equality of canonical
bytes), or the default when none does. A Missing key, such as an absent
optional field, also selects the default: `lookup(input.hint, [{when: 4, then:
4}], default 0)` is 0 when `hint` is absent. Only the selected expression is
evaluated: the others are not evidence for the result, so an unresolved
reference in an unselected case or default does not propagate. At most 256
cases; each `when` is at most 1024 canonical bytes; no two `when` are
canonically equal. Each `when` must be a value the key can take: of the key's
kind and shape, and in the key's string enum when it has one (an integer key
accepts any integer, a string key any length). The result type is the union of
every case's `then` and the default (string results merge into one enum); past
16 alternatives the lookup is refused. Checked against a target schema, each
result meets the target itself, as a literal does. An empty array, `[]` or an
array expression with no items, has no item type of its own and fits any array
type, also when it is merged with other results.

`let` binds names in order; each binding sees the ones before it, and the body
sees all of them:

    {"op":"let","bindings":[{"name":"a","value":{"op":"literal","value":5}},
                            {"name":"b","value":{"op":"literal","value":7}}],
     "body":{"op":"lookup","key":{"op":"lt","left":{"op":"ref","root":"a"},
                                  "right":{"op":"ref","root":"b"}},
             "cases":[{"when":true,"then":{"op":"ref","root":"b"}}],
             "default":{"op":"ref","root":"a"}}}

is `max(a, b)`, 7, with no arithmetic. Each binding is evaluated exactly once,
however often it is used. A binding that is Unknown makes the whole `let`
Unknown, even when the body never reads it, as `and(false, unknown)` stays
unknown. A Missing binding is bound as Missing, so `exists` reads it as false
and any other use propagates it exactly as the inlined reference would. At
most 16 bindings. A name is a valid identifier and is never `input`, `event`,
`definitions` or `vars`, a step binding or iteration item anywhere in the
playbook, or a name of an enclosing `let` or collection item, including an
earlier binding of the same `let`.

Evaluation stops past depth 32 and past 10 000 elements: one per expression
evaluated, one per lookup case whether selected or not, and a map or filter body
once per item. Validation refuses an expression that uses `lookup` or `let`
when, on its worst path (every map or filter at its `max_items`, the costliest
lookup branch), it could exceed either limit, so such an expression never fails
on them at run time. A 256-case lookup on a plain key, inside a map, therefore
fits a `max_items` of at most 38. The compiler also holds every expression of a new draft
to the depth limit; verifying a stored artifact does not, so artifacts stored
before these rules keep loading. It also bounds a new draft's size (see "What a
draft is held to", rule 4).

Diagnostics for both name the place inside the step, for example
`steps.route.input.level.lookup.cases[0].when`: object fields by name, array
items as `[i]`, and every other expression by its `op` followed by its operand.

Both expressions are part of format 1: `FORMAT_VERSION` stays 1, and an
artifact that does not use them keeps its canonical bytes, digest and
executable fingerprint. A runner built before them refuses an artifact that
uses them when it deserializes it (unknown `op`).

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

A member is named by its binding, with no path. An owned child is
`{"op":"ref","root":"<bind>"}` or `{"op":"ref","root":"<bind>","path":["child"]}`.
An operation handle is `{"op":"ref","root":"<bind>"}` only: the binding the invoke
step's `accepted` edge recorded (`{"handle": h}`). `<bind>` may also be written
`{"op":"ref","root":"vars","path":["<bind>"]}`. Compiling a draft refuses a path
inside an operation handle, such as `["handle"]`, with `join_member_type`, in
joins with or without `member_keys`. The runner refuses that member when the run
reaches the join. Verifying a stored artifact still accepts it, so an artifact
stored before this rule keeps loading.

What a draft is held to. Compiling a new draft applies four rules that
verifying a stored artifact does not, so an artifact stored before them keeps
loading. They follow from how a value that is not there behaves. A reference to
it evaluates to Missing, and Missing travels: an object, an array, a `map`, a
`filter`, a comparison or a `let` body with a Missing part is Missing itself.
Only `exists` stops it, by answering `false`. A step that needs a Missing value
stops the run ("expression input missing"), a `decide` takes `unknown`, and a
`lookup` whose key is Missing takes its default. Ordinary typing does not see
any of this: it merges the outcomes a binding can hold into one object, in which
a field only one outcome has is merely optional, and an optional field types as
present. The draft check keeps each binding's outcomes apart, on every way of
reaching each step.

1. Strict reads. A reference is refused with `expression_type` when it reads
   through a field that some outcome lacks:
   - an invoke or call outcome without the field, where several outcome edges
     meet at one step;
   - an outcome without the field among a join's accepted outcomes, which is
     every outcome when `accepted_outcomes` is empty;
   - a join key that can be absent: on `timeout`, a member that has not
     answered, and in modes `any_accepted` and `any_completion` with several
     members, a member that was not the one selected.

   A field that is merely optional in its one schema stays readable. So does
   a position of an array a schema describes: no schema states a length, so
   the read relies on the contract that returns the array.

   A literal is read as the value it is, exactly as the same value written
   with `object` and `array` expressions: each row of a literal table keeps
   its own fields, and a literal array its own length. A read of a field in a
   row that lacks it, or of a position past the end, is refused; so is a
   `map`, `filter` or `lookup` key that reads the field in every row when one
   row lacks it. Give every row of a table the same fields.

2. Bound reads. A reference to a binding is refused at a step that some way
   reaches without binding it:
   - through the binding step's `on_error` (the operation failed, so nothing
     was bound), or a wait's `timeout` edge;
   - around the binding step: from a branch that skips it, on the first pass of
     a loop that binds it later, after a `for_each` whose body binds it (the
     collection may be empty), or at an entry step reached again by a back edge;
   - a `for_each` item outside the loop's body: it is bound for the body only,
     and not on the loop's `next` or `on_error`, nor in the loop's own
     `collection`.

   Inside a `for_each` body a failing step is handled by the loop, not by the
   step's own `on_error`: with `on_failure: continue` the loop goes on to the
   next item, with `stop` it leaves by its own `on_error`. The check follows
   both.

3. Aliasing does not launder. Whatever holds such a value carries it, and a
   read through it is refused exactly as the direct read would be, with the
   same reasons: a `let` name, a name for a name, an object or array that holds
   it, a `lookup` that can return it (as a case or as the default), the result
   of a `map`, `filter` or `difference` over it and the items of that result, a
   `map` or `filter` item, and a `for_each` item in the loop's body. A Missing
   value bound to a `let` name does nothing until the name's value is needed,
   so a name used only inside `exists` is fine; where it is needed, the
   diagnostic points at the reference that was Missing and names the name it
   was used through.

   The one exemption from rules 1 to 3 is the operand of `exists`. A `lookup`
   key is not exempt, even inside `exists`: a Missing key takes the lookup's
   default, so an exemption would let a playbook route on a default without a
   trace. A field merely optional in its one schema may be a lookup key (that
   is what the default is for). The identity of a `difference` is not exempt
   either: it is read in every item, and an item without it stops the run.

4. Bounded cost. A draft is refused, before anything costly runs on it, when
   it has more than 256 steps, a `draft.json` over 786 432 bytes (pretty-printed
   JSON is fine), an expression that counts more than 200 000 nodes,
   expressions that count more than 1 000 000 nodes in all, or more than
   250 000 type nodes in scope over all its steps. The purpose: nothing a draft
   within the bounds seals into is expensive to check or to verify on every
   load (in a release build: at most 2 s and 256 MB to check, 1.5 s and 256 MB
   to verify).
   - The size of an expression is its own nodes, with a literal counted by
     the nodes of its value, and every reference counted by the nodes of the
     type it reads: `task.facts.kind` counts one node, `task` its whole result
     type. That holds for a reference to `input`, `event`, `definitions`,
     `vars`, a step's binding or a `for_each` item, and equally for a `let`
     name and a `map` or `filter` item. What a name stands for is counted
     once, where it is written.
   - So a table costs what it is written with, once: bind it or its result to
     a `let` name and read the name as often as needed. Reading a name whose
     value is small (a level, a flag) counts one node each time; reading a
     whole table or a whole binding counts all of it each time, so read the one
     field you need. A value built from the same name twice at every level
     doubles its type each time: fifteen levels are about 200 000 nodes,
     and sixteen are refused.
   - A value that can have many shapes (literal rows with differing fields, a
     lookup with many differently shaped results) costs one step to follow
     for each shape at every read. A read counts at least that, and a draft
     that follows more than 1 000 000 shapes in all is refused with
     `draft_limit`.
   - The scope count is, for each step an entry reaches, the type of that
     entry's `input`, the captured definitions, and the result type of every
     step that binds one. Ordinary typing copies these for every step, on every
     load. With several entries over the same steps, the steps and their
     expressions count once for each entry.
   - The diagnostic (`expression_limit` or `draft_limit`) gives the number that
     exceeded and names the `let` name or root that contributes most, with its
     size and how often it is read.
   - The byte bound is not 1 048 576: verifying an artifact that holds a
     literal of very many one-field rows costs about 2 KB a row, and at
     786 432 bytes the worst such draft stays inside the memory budget.

A read diagnostic names the step and the place inside it, the reference as
written, what a name in it stands for, every way that lacks the field or the
binding, and the remedy. Read the value only where it is always present: after
the outcome edge that returns it, or on the `completed` edge of a join whose
`accepted_outcomes` lists only outcomes that have it. A `decide` on `outcome`
does not narrow anything. `exists` only tells you whether the value is there:
testing it does not make a read of the value allowed, in the same expression
(`lookup(exists(x), [{when: true, then: x}], ...)` is refused at `x`) or in a
later step, because the check does not narrow by a test. Use `exists` on its
own, as the whole test of a `decide` or a lookup key, to route to a step that
does not read the value; to use the value, branch on the edge first and read
it in the step that edge leads to. At most twelve ways are listed.

Ask, then read the answer. A wait on an asynchronous command's completion event
binds the event's whole payload, which a wait does not narrow per outcome. To read
the answer, join the operation handle by name instead. `ask` invokes catalog
2.1.0's `judgment.request`. Its `accepted` edge binds the handle as `j`. `read`
joins `j` under the key `c` and accepts only `assessed`. On `completed`,
`answer.c.output` is exactly the `assessed` output, so `decide` can branch on
`choice`, a field only that outcome has:

    "ask":    {"op":{"kind":"invoke","bind":"j","retry":null,
                     "command":{"name":"judgment.request","fingerprint":"sha256:…"},
                     "input":{"op":"literal","value":{"question":"Should this change be approved?",
                       "subject":{"id":"work-1","revision":1},
                       "context":{"text":"Work item summary.","artifacts":[]},
                       "output":{"choices":["approve","decline"],"rationale_max_bytes":2048},
                       "budget":{"deadline_ms":60000,"max_cost_microunits":50000,
                                 "max_input_tokens":8192,"max_output_tokens":512}}}},
               "edges":{"accepted":"read","assessed":"error","failed":"error",
                        "refused":"error"},
               "on_error":"error"},
    "read":   {"op":{"kind":"join","members":[{"op":"ref","root":"j","path":[]}],
                     "member_keys":["c"],"mode":"all","accepted_outcomes":["assessed"],
                     "deadline_ms":60000,"remaining":"track","bind":"answer"},
               "edges":{"completed":"decide","no_match":"unanswered","timeout":"late"},
               "on_error":"error"},
    "decide": {"op":{"kind":"decide","predicate":{"op":"eq",
                     "left":{"op":"ref","root":"answer","path":["c","output","choice"]},
                     "right":{"op":"literal","value":"approve"}}},
               "edges":{"true":"approved","false":"declined","unknown":"error"},
               "on_error":null}

`approved`, `declined`, `unanswered`, `late` and `error` are `return` steps. Each
edge sees a different answer:
- `completed`: the answer was `assessed`. `answer.c.outcome` is `"assessed"` and
  `answer.c.output.choice` is a string of at most 512 bytes.
- `no_match`: the answer was `failed` or `refused`. `answer.c.output` is typed
  null-or-any-outcome, so reading `answer.c.output.choice` there is refused with
  `expression_type`. The diagnostic names the step, the reference and why it
  cannot be read.
- `timeout`: no answer arrived within `deadline_ms`. `answer` is `{}`.

An answer that arrives before the run reaches the join is recorded on the
operation, and the join reads it when the run arrives. An answer accepted at
the join's deadline still counts. After a timeout, the run holds its return
(status `finishing`) until the operation it owns settles. The answer, however
late, settles the operation without changing the outcome. If no answer ever
arrives, the operation becomes unknown at the command's own deadline (60 s for
`judgment.request`), and the run stays live. The engine does not settle it by
itself. `judgment.request` cannot be cancelled, so `"remaining":"cancel"` does not
help, and as a `keyed_idempotent` command it is replayed only on an authorized
recovery request. Bounding such a run is the host's duty: the host must always
deliver a final completion (for example, `failed` when its own budget deadline
passes).

Callable dependencies declare `selection`: `exact` uses the captured artifact
hash; `catalog` resolves the named binding at root admission. A catalog allowlist
may constrain acceptable full artifact hashes; an empty allowlist permits the
current interface-compatible version. The captured artifact remains compilation
provenance. Compatible implementations accept every declared caller input and
produce only declared caller outcomes with assignable output schemas. Admission
also validates scope, current caller authority, and the entire selected acyclic
closure against one catalog revision, then freezes exact hashes for all children.
Changing activation cannot retarget an already admitted root or its grandchildren.
