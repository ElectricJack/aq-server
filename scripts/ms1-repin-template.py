#!/usr/bin/env python3
"""Re-pin the ms1 stack's sandbox template image and/or session-host helper in the vault config.

The template digest and the helper pin (`aq.ms1-host/aq-session-host`: digest, bytes, path)
appear inside several JSON definition bodies in `[profiled_execution] definition_bodies`. Each
body's artifact digest is the sha256 of its bytes, other bodies, `qualified_runtimes` and
`[intelligence] qualified_harnesses` reference bodies as `{id, version, digest}`, and the server
keeps definitions by id+version and refuses changed content under an old version
(`admission_definition_identity_conflict`). So a changed body gets a new version, every
reference follows, and both section revisions increase. The node template follows the same
references.

  scripts/ms1-repin-template.py CONFIG NATIVE OUT_DIR \\
      [--template-digest NEW_HEX --old-template-digest OLD_HEX] [--helper-path PATH]

CONFIG and NATIVE are the current config.toml and native.json. OUT_DIR receives config.toml,
native.json, composite.json, codex-composite.json (when the config has that body) and
inventory.json, each mode 0600. Nothing is changed in place. The command refuses when neither
a template digest nor a helper path is given, or when nothing would change. It prints ids,
versions and digests only.
"""
import argparse
import copy
import hashlib
import json
import re
import sys
import tomllib
from pathlib import Path

HELPER = "aq.ms1-host/aq-session-host"
COMPOSITE = "aq.claude-result-local-adapter"
CODEX_COMPOSITE = "aq.codex-docker-result-local-adapter"
INVENTORY = "aq.scoped-material-presence-qualification/ms1"
HEX64 = re.compile(r"[0-9a-f]{64}")


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def sha(data):
    return hashlib.sha256(data if isinstance(data, bytes) else data.encode()).hexdigest()


def toml_value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, str):
        return json.dumps(v, ensure_ascii=False)
    if isinstance(v, list):
        return "[" + ", ".join(toml_value(x) for x in v) + "]"
    if isinstance(v, dict):
        return "{" + ", ".join(f"{json.dumps(k)} = {toml_value(x)}" for k, x in sorted(v.items())) + "}"
    raise ValueError(f"cannot write {type(v).__name__} as TOML")


def arguments(argv=None):
    p = argparse.ArgumentParser(description="Re-pin the ms1 sandbox template and/or session-host helper.")
    p.add_argument("config", type=Path, help="current vault config.toml")
    p.add_argument("native", type=Path, help="current native.json")
    p.add_argument("out", type=Path, help="output directory")
    p.add_argument("--template-digest", help="new sandbox template digest (64 lowercase hex)")
    p.add_argument("--old-template-digest", help="template digest to replace (64 lowercase hex); required with --template-digest")
    p.add_argument("--helper-path", type=Path, help="new session-host helper binary")
    args = p.parse_args(argv)
    if args.template_digest is None and args.helper_path is None:
        p.error("give --template-digest and/or --helper-path: nothing to change")
    if (args.template_digest is None) != (args.old_template_digest is None):
        p.error("--template-digest and --old-template-digest go together")
    for name in ("template_digest", "old_template_digest"):
        value = getattr(args, name)
        if value is not None and not HEX64.fullmatch(value):
            p.error(f"--{name.replace('_', '-')} must be 64 lowercase hex characters")
    if args.template_digest == args.old_template_digest and args.template_digest is not None:
        p.error("--template-digest equals --old-template-digest")
    return args


def make_edit(old_template, new_template, helper):
    """Point the template at the new image and every helper pin at the new helper."""
    def edit(v):
        if isinstance(v, dict):
            if helper is not None and isinstance(v.get("artifact"), dict) and v["artifact"].get("id") == HELPER:
                return dict(helper) if "bytes" in v else {"artifact": helper["artifact"], "path": helper["path"]}
            return {k: edit(x) for k, x in v.items()}
        if isinstance(v, list):
            return [edit(x) for x in v]
        if new_template is not None and isinstance(v, str) and old_template in v:
            return v.replace(old_template, new_template)
        return v
    return edit


def refs_replace(v, mapping):
    if isinstance(v, dict):
        if set(v) == {"id", "version", "digest"} and (v["id"], v["version"], v["digest"]) in mapping:
            return dict(mapping[(v["id"], v["version"], v["digest"])])
        return {k: refs_replace(x, mapping) for k, x in v.items()}
    if isinstance(v, list):
        return [refs_replace(x, mapping) for x in v]
    return v


def repin(text, native_value, old_template, new_template, helper_path):
    """Return (config text, native value, bodies by id, mapping, version, profiled, intelligence)."""
    config = tomllib.loads(text)
    profiled, intelligence = config["profiled_execution"], config["intelligence"]
    helper = None
    if helper_path is not None:
        hb = helper_path.read_bytes()
        helper = {"artifact": {"digest": sha(hb), "id": HELPER, "version": "1"}, "bytes": len(hb), "path": str(helper_path)}
    edit = make_edit(old_template, new_template, helper)

    bodies = []
    for d in profiled["definition_bodies"]:
        try:
            value = json.loads(d["body"])
        except ValueError:
            value = None
        bodies.append({"artifact": dict(d["artifact"]), "kind": d["kind"], "body": d["body"], "value": value,
                       "original": copy.deepcopy(value), "old": dict(d["artifact"])})
    for b in bodies:
        if b["value"] is not None:
            b["value"] = edit(b["value"])
    versions = [int(b["artifact"]["version"]) for b in bodies if b["artifact"]["version"].isdigit()]
    version = str(max(versions, default=0) + 1)

    mapping = {}
    for _ in range(30):
        changed = False
        for b in bodies:
            if b["value"] is None:
                continue
            b["value"] = refs_replace(b["value"], mapping)
            if b["value"] == b["original"]:
                continue
            body = canonical(b["value"])
            new = {"id": b["artifact"]["id"], "version": version, "digest": sha(body)}
            if new != b["artifact"] or body != b["body"]:
                mapping[(b["old"]["id"], b["old"]["version"], b["old"]["digest"])] = new
                b["artifact"], b["body"], changed = new, body, True
        if not changed:
            break
    else:
        raise SystemExit("references did not settle")
    if not mapping:
        raise SystemExit("nothing changes: no definition body holds the template digest or a helper pin to replace")
    for b in bodies:
        assert sha(b["body"]) == b["artifact"]["digest"] or b["value"] is None, b["artifact"]["id"]
    if new_template is not None and any(old_template in b["body"] for b in bodies):
        raise SystemExit("the old template digest is still in a definition body")

    profiled["definition_bodies"] = [{"artifact": b["artifact"], "body": b["body"], "kind": b["kind"]} for b in bodies]
    profiled["qualified_runtimes"] = edit(refs_replace(profiled["qualified_runtimes"], mapping))
    intelligence["qualified_harnesses"] = edit(refs_replace(intelligence["qualified_harnesses"], mapping))
    profiled["revision"] += 1
    intelligence["revision"] += 1

    lines, section, done = text.split("\n"), None, set()
    for n, line in enumerate(lines):
        if line.startswith("["):
            section = line.strip()
        for table, obj, key in (("[intelligence]", intelligence, "qualified_harnesses"), ("[intelligence]", intelligence, "revision"),
                                ("[profiled_execution]", profiled, "definition_bodies"), ("[profiled_execution]", profiled, "qualified_runtimes"),
                                ("[profiled_execution]", profiled, "revision")):
            if section == table and line.startswith(key + " = "):
                lines[n] = f"{key} = {toml_value(obj[key])}"
                done.add((table, key))
    if len(done) != 5:
        raise SystemExit(f"config lines not found (each key must sit on one line in its table): {sorted(done)}")
    new_text = "\n".join(lines)
    check = tomllib.loads(new_text)
    skip = ("profiled_execution", "intelligence")
    if check["profiled_execution"] != profiled or check["intelligence"] != intelligence:
        raise SystemExit("the rewritten config does not parse back to the intended sections")
    if {k: v for k, v in check.items() if k not in skip} != {k: v for k, v in config.items() if k not in skip}:
        raise SystemExit("the rewrite changed a section it must not touch")
    if new_template is not None and old_template in new_text:
        raise SystemExit("the old template digest is still in the config")

    native = edit(refs_replace(native_value, mapping))
    if new_template is not None and old_template in json.dumps(native):
        raise SystemExit("the old template digest is still in native.json")
    return new_text, native, {b["artifact"]["id"]: b for b in bodies}, mapping, version, profiled, intelligence


def main(argv=None):
    args = arguments(argv)
    outputs = {args.out / n for n in ("config.toml", "native.json")}
    if any(p.exists() and (p.resolve() in (args.config.resolve(), args.native.resolve())) for p in outputs):
        raise SystemExit("OUT_DIR would overwrite an input file")
    text = args.config.read_text()
    native_value = json.loads(args.native.read_text())
    new_text, native, by_id, mapping, version, profiled, intelligence = repin(
        text, native_value, args.old_template_digest, args.template_digest, args.helper_path)
    for needed in (COMPOSITE, INVENTORY):
        if needed not in by_id:
            raise SystemExit(f"the config has no definition body {needed}")

    args.out.mkdir(mode=0o700, exist_ok=True)

    def write(name, data):
        p = args.out / name
        p.write_text(data)
        p.chmod(0o600)

    write("config.toml", new_text)
    write("native.json", canonical(native))
    write("composite.json", by_id[COMPOSITE]["body"])
    if CODEX_COMPOSITE in by_id:
        write("codex-composite.json", by_id[CODEX_COMPOSITE]["body"])
    write("inventory.json", by_id[INVENTORY]["body"])

    for (ident, old_version, old_digest), new in sorted(mapping.items()):
        print(f"{ident} v{old_version} {old_digest[:12]} -> v{new['version']} {new['digest'][:12]}")
    print("version", version, "changed", len(mapping), "pe rev", profiled["revision"], "intel rev", intelligence["revision"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
