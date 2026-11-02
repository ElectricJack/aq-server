# Human phase result reads: isolated qualification

Final source `451e51a29c42645d7ccda97dc2dfc5b11a61dc75`, base `cd64196`.
Independent Decisions review accepted the source after full-principal fencing.

- phase.log: six actual disposable SQL/router cases,24.62s;18 unrelated phase cases filtered.
- legacy.log: two legacy cases,1.75s;47 unrelated cases filtered.
- strict.log: both affected targets with -D warnings,39.09s.
- earlier-functional.log: four earlier passes,18.32s, before the identity fix; retained as an earlier increment.
- qualification.json: commands, hashes, actual counts and limitations.

The fixtures use actual SQL/result/origin owners and protected routers, with
explicitly synthetic proof/source/runtime DTOs. Privileged disposable corruption
is only a refusal vector. Each nominal family proves a real common Node-lock
wait, Owner identity rebind/ABA restoration, stale403/fresh200 and unchanged
authority. Default v1 remains legacy-only, v2 has honest coverage and phase media
remains unavailable. No physical harness/provider/acceptance/settlement or
completion is established. No canonical service, database or UI was changed.

Commands use jobs2/offline/dev+test debug0/incremental0, the existing SERVER cache
and existing loopback test PostgreSQL. No executable was copied or pinned.
SHA256SUMS covers every retained file except itself.
