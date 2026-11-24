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
from typing import NamedTuple

HELPER = "aq.ms1-host/aq-session-host"
COMPOSITE = "aq.claude-result-local-adapter"
CODEX_COMPOSITE = "aq.codex-docker-result-local-adapter"
INVENTORY = "aq.scoped-material-presence-qualification/ms1"
HEX64 = re.compile(r"[0-9a-f]{64}")


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def sha(data):
    return hashlib.sha256(data if isinstance(data, bytes) else data.encode()).hexdigest()


def toml_items(table):
    return ", ".join(f"{json.dumps(k)} = {toml_value(x)}" for k, x in sorted(table.items()))


def toml_value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, str):
        return json.dumps(v, ensure_ascii=False)
    if isinstance(v, list):
        return "[" + ", ".join(map(toml_value, v)) + "]"
    if isinstance(v, dict):
        return "{" + toml_items(v) + "}"
    raise ValueError(f"cannot write {type(v).__name__} as TOML")


def validate_digests(parser, args):
    for name in ("template_digest", "old_template_digest"):
        value = getattr(args, name)
        if value is not None and not HEX64.fullmatch(value):
            parser.error(f"--{name.replace('_', '-')} must be 64 lowercase hex characters")


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
    validate_digests(p, args)
    if args.template_digest == args.old_template_digest and args.template_digest is not None:
        p.error("--template-digest equals --old-template-digest")
    return args


def helper_reference(v):
    artifact = v.get("artifact") if isinstance(v, dict) else None
    return isinstance(artifact, dict) and artifact.get("id") == HELPER


def replace_helper(helper, value):
    if "bytes" in value:
        return dict(helper)
    return {"artifact": helper["artifact"], "path": helper["path"]}


def map_json(value, replace):
    updated = replace(value)
    if updated is not value:
        return updated
    if isinstance(value, dict):
        return {k: map_json(child, replace) for k, child in value.items()}
    if isinstance(value, list):
        return [map_json(child, replace) for child in value]
    return value


def make_edit(old_template, new_template, helper):
    """Point the template at the new image and every helper pin at the new helper."""
    def replace(value):
        if isinstance(value, str) and new_template is not None:
            return value.replace(old_template, new_template)
        if helper is not None and helper_reference(value):
            return replace_helper(helper, value)
        return value

    return lambda value: map_json(value, replace)


def refs_replace(value, mapping):
    def replace(item):
        if not isinstance(item, dict) or set(item) != {"id", "version", "digest"}:
            return item
        replacement = mapping.get((item["id"], item["version"], item["digest"]))
        return dict(replacement) if replacement is not None else item

    return map_json(value, replace)


def helper_pin(helper_path):
    helper = None
    if helper_path is not None:
        hb = helper_path.read_bytes()
        helper = {"artifact": {"digest": sha(hb), "id": HELPER, "version": "1"}, "bytes": len(hb), "path": str(helper_path)}
    return helper


def load_body(definition):
    try:
        value = json.loads(definition["body"])
    except ValueError:
        value = None
    artifact = definition["artifact"]
    return {"artifact": dict(artifact), "kind": definition["kind"], "body": definition["body"],
            "value": value, "original": copy.deepcopy(value), "old": dict(artifact)}


def next_version(bodies):
    versions = (b["artifact"]["version"] for b in bodies)
    return str(max((int(v) for v in versions if v.isdigit()), default=0) + 1)


def update_body(record, edited, mapping, version):
    if edited is None:
        return False
    record["value"] = refs_replace(edited, mapping)
    if record["value"] == record["original"]:
        return False
    body = canonical(record["value"])
    new = {"id": record["old"]["id"], "version": version, "digest": sha(body)}
    if new == record["artifact"] and body == record["body"]:
        return False
    original = record["old"]
    mapping[original["id"], original["version"], original["digest"]] = new
    record["artifact"], record["body"] = new, body
    return True


def settle_definitions(bodies, edit, version):
    # Each pass resolves the original reference IDs. Intermediate digests may change on a later pass.
    edited = [edit(b["original"]) for b in bodies]
    mapping = {}
    for _ in range(30):
        changed = False
        for record, value in zip(bodies, edited):
            if update_body(record, value, mapping, version):
                changed = True
        if not changed:
            return mapping
    raise SystemExit("references did not settle")


def validate_body_digest(body):
    if body["value"] is not None:
        assert sha(body["body"]) == body["artifact"]["digest"], body["artifact"]["id"]


def validate_bodies(bodies, mapping, old_template, new_template):
    if not mapping:
        raise SystemExit("nothing changes: no definition body holds the template digest or a helper pin to replace")
    for b in bodies:
        validate_body_digest(b)
    if new_template is not None and any(old_template in b["body"] for b in bodies):
        raise SystemExit("the old template digest is still in a definition body")


def update_sections(config, bodies, mapping, edit):
    profiled, intelligence = config["profiled_execution"], config["intelligence"]
    profiled["definition_bodies"] = [{"artifact": b["artifact"], "body": b["body"], "kind": b["kind"]} for b in bodies]
    profiled["qualified_runtimes"] = edit(refs_replace(profiled["qualified_runtimes"], mapping))
    intelligence["qualified_harnesses"] = edit(refs_replace(intelligence["qualified_harnesses"], mapping))
    profiled["revision"] += 1
    intelligence["revision"] += 1


def changed_config_values(config):
    keys = {"intelligence": ("qualified_harnesses", "revision"),
            "profiled_execution": ("definition_bodies", "qualified_runtimes", "revision")}
    return {(f"[{table}]", key): config[table][key] for table in keys for key in keys[table]}


def rewrite_config(text, config):
    values = changed_config_values(config)
    lines, section, done = text.split("\n"), None, set()
    for n, line in enumerate(lines):
        if line.startswith("["):
            section = line.strip()
        key, separator, _ = line.partition(" = ")
        if separator and (section, key) in values:
            lines[n] = f"{key} = {toml_value(values[section, key])}"
            done.add((section, key))
    if len(done) != 5:
        raise SystemExit(f"config lines not found (each key must sit on one line in its table): {sorted(done)}")
    return "\n".join(lines)


def untouched_sections(config):
    return {k: v for k, v in config.items() if k not in ("profiled_execution", "intelligence")}


def validate_sections(check, config):
    for section in ("profiled_execution", "intelligence"):
        if check[section] != config[section]:
            raise SystemExit("the rewritten config does not parse back to the intended sections")
    if untouched_sections(check) != untouched_sections(config):
        raise SystemExit("the rewrite changed a section it must not touch")


def validate_config(new_text, config, old_template, new_template):
    validate_sections(tomllib.loads(new_text), config)
    if new_template is not None and old_template in new_text:
        raise SystemExit("the old template digest is still in the config")


class TemplateUpdate(NamedTuple):
    old_digest: str | None
    new_digest: str | None
    helper_path: Path | None


def repin(text, native_value, update):
    """Return (config text, native value, bodies by id, mapping, version, profiled, intelligence)."""
    config = tomllib.loads(text)
    profiled, intelligence = config["profiled_execution"], config["intelligence"]
    edit = make_edit(update.old_digest, update.new_digest, helper_pin(update.helper_path))
    bodies = [load_body(d) for d in profiled["definition_bodies"]]
    version = next_version(bodies)
    mapping = settle_definitions(bodies, edit, version)
    validate_bodies(bodies, mapping, update.old_digest, update.new_digest)
    update_sections(config, bodies, mapping, edit)
    new_text = rewrite_config(text, config)
    validate_config(new_text, config, update.old_digest, update.new_digest)
    native = edit(refs_replace(native_value, mapping))
    if update.new_digest is not None and update.old_digest in json.dumps(native):
        raise SystemExit("the old template digest is still in native.json")
    return new_text, native, {b["artifact"]["id"]: b for b in bodies}, mapping, version, profiled, intelligence


def check_output_paths(args):
    outputs = {args.out / n for n in ("config.toml", "native.json")}
    if any(p.exists() and (p.resolve() in (args.config.resolve(), args.native.resolve())) for p in outputs):
        raise SystemExit("OUT_DIR would overwrite an input file")


def write_outputs(out, new_text, native, by_id):
    for needed in (COMPOSITE, INVENTORY):
        if needed not in by_id:
            raise SystemExit(f"the config has no definition body {needed}")

    out.mkdir(mode=0o700, exist_ok=True)

    def write(name, data):
        p = out / name
        p.write_text(data)
        p.chmod(0o600)

    write("config.toml", new_text)
    write("native.json", canonical(native))
    write("composite.json", by_id[COMPOSITE]["body"])
    if CODEX_COMPOSITE in by_id:
        write("codex-composite.json", by_id[CODEX_COMPOSITE]["body"])
    write("inventory.json", by_id[INVENTORY]["body"])


def main(argv=None):
    args = arguments(argv)
    check_output_paths(args)
    text = args.config.read_text()
    native_value = json.loads(args.native.read_text())
    update = TemplateUpdate(args.old_template_digest, args.template_digest, args.helper_path)
    new_text, native, by_id, mapping, version, profiled, intelligence = repin(text, native_value, update)
    write_outputs(args.out, new_text, native, by_id)
    for (ident, old_version, old_digest), new in sorted(mapping.items()):
        print(f"{ident} v{old_version} {old_digest[:12]} -> v{new['version']} {new['digest'][:12]}")
    print("version", version, "changed", len(mapping), "pe rev", profiled["revision"], "intel rev", intelligence["revision"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
