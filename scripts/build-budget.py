#!/usr/bin/env python3
"""Enforce the ratcheted build budgets in build-budget.toml.

Two numbers decide how slow this workspace is to build and test, and both only
ever grew until they were measured (see AGENTS.md "Build health"):

* integration test binaries: every `tests/*.rs` file (or `[[test]]` target) is
  a separate executable that links the whole library and its dependencies
  (aq-server declares a few domain binaries, `tests/<group>/main.rs`, with
  `autotests = false`);
* library size: the lines of each crate's lib, which rustc compiles as one unit.

Both may only go down. The check measures the base branch (origin/main by
default) and this tree, and fails when this tree has more test binaries, or more
lines in a crate that has a ratchet, than the base. So a pull request never edits
build-budget.toml to keep the gain, and two pull requests never conflict on it.

The numbers in build-budget.toml are ceilings. Growth is allowed only when this
tree raises a ceiling above the base's value, an explicit, reviewed edit that
says why; the tree may then grow up to the new ceiling. Without a base (no
origin/main), the ceilings alone are checked.

    python3 scripts/build-budget.py                  # check against origin/main (CI lint job)
    python3 scripts/build-budget.py --base REF       # check against another ref
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BUDGET = ROOT / "build-budget.toml"


def metadata(root=ROOT):
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--offline"],
        cwd=root, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def lib_lines(package):
    """Lines of .rs source under the lib target's directory, excluding other targets."""
    targets = package["targets"]
    library_kinds = {"lib", "proc-macro"}
    lib = next((t for t in targets if not library_kinds.isdisjoint(t["kind"])), None)
    if lib is None:
        return None
    src = Path(lib["src_path"]).parent
    others = {Path(t["src_path"]) for t in targets if t is not lib}
    binaries = src / "bin"
    total = 0
    for path in sorted(src.rglob("*.rs")):
        if path in others or binaries in path.parents:
            continue
        with open(path, "rb") as handle:
            total += sum(1 for _ in handle)
    return total


def measure(root=ROOT):
    meta = metadata(root)
    members = set(meta["workspace_members"])
    packages = [p for p in meta["packages"] if p["id"] in members]
    test_binaries, unbuilt = measure_tests(packages, root)
    lines = {}
    for package in packages:
        count = lib_lines(package)
        if count is not None:
            lines[package["name"]] = count
    return test_binaries, lines, unbuilt


def measure_tests(packages, root):
    count = 0
    unbuilt = []
    for package in packages:
        tests = Path(package["manifest_path"]).parent / "tests"
        declared = [target for target in package["targets"] if "test" in target["kind"]]
        target_paths = {Path(target["src_path"]).resolve() for target in declared}
        count += len(declared)
        candidates = sorted([*tests.glob("*.rs"), *tests.glob("*/main.rs")])
        unbuilt.extend(str(path.relative_to(root)) for path in candidates
                       if path.resolve() not in target_paths)
    return count, unbuilt


def base_state(ref):
    """(test binaries, lib lines, budget) of `ref`, or None when the ref is missing."""
    resolved = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", "--end-of-options", f"{ref}^{{commit}}"],
        cwd=ROOT, capture_output=True, text=True,
    )
    if resolved.returncode != 0:
        return None
    with tempfile.TemporaryDirectory(prefix="build-budget-base-") as tmp:
        tree = Path(tmp) / "base"
        tree.mkdir()
        # Give the base its own index and checkout directory for Cargo metadata.
        checkout = dict(os.environ, GIT_INDEX_FILE=str(Path(tmp) / "index"),
                        GIT_WORK_TREE=str(tree))
        subprocess.run(["git", "read-tree", resolved.stdout.strip()],
                       cwd=ROOT, env=checkout, check=True, capture_output=True)
        subprocess.run(["git", "checkout-index", "--all", f"--prefix={tree}/"],
                       cwd=ROOT, env=checkout, check=True, capture_output=True)
        test_binaries, lines, _ = measure(tree)
        budget = tomllib.loads((tree / BUDGET.name).read_text())
    return test_binaries, lines, budget


def allowed(base_count, ceiling, base_ceiling):
    """The most this tree may have: the base's count, or a ceiling this tree raised."""
    if base_count is None:
        return ceiling
    if base_ceiling is not None and ceiling > base_ceiling:
        return max(base_count, ceiling)
    return base_count


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", default="origin/main", help="ref to compare with (default origin/main)")
    if "--update" in sys.argv[1:]:
        print("build-budget: --update is gone: the check compares with origin/main, so a lower count "
              "needs no edit of build-budget.toml. Run the script without arguments.", file=sys.stderr)
        return None
    return parser.parse_args()


def check_tests(test_binaries, unbuilt, test_budget, base):
    base_tests, _, base_budget = base
    errors = []
    test_limit = allowed(base_tests, test_budget, base_budget.get("test_binaries", {}).get("max"))
    print(f"build-budget: integration test binaries {test_binaries} (limit {test_limit}, ceiling {test_budget})")
    if test_binaries > test_limit:
        errors.append(
            f"{test_binaries} integration test binaries, more than {test_limit}. "
            "Add new tests to an existing test binary (for aq-server, a module of an "
            "existing tests/<group>/main.rs) instead of a new test target.")

    for path in unbuilt:
        errors.append(
            f"{path} is not a test target, so it is never built or run (the crate sets "
            "autotests = false). Make it a module of an existing tests/<group>/main.rs.")
    return errors


def check_libraries(lines, budget, base, ref):
    _, base_lines, base_budget = base
    line_target = budget["target"]
    ratchets = budget.get("ratchet", {})
    base_ratchets = base_budget.get("lib_lines", {}).get("ratchet", {})
    ratchet_context = f"on {ref} (target {line_target})"
    errors = []
    for name, count in sorted(lines.items(), key=lambda item: -item[1]):
        ceiling = ratchets.get(name)
        if ceiling is None:
            error = check_line_target(name, count, line_target)
        else:
            limit = allowed(base_lines.get(name), ceiling, base_ratchets.get(name))
            # An owner-granted allowance on main lets every pull request grow up to it.
            limit = max(limit, budget.get("allowance", {}).get(name, 0))
            print(f"build-budget: {name} lib {count} lines (limit {limit}, ceiling {ceiling}, target {line_target})")
            error = ratchet_error(name, count, limit, ratchet_context)
        if error:
            errors.append(error)
    for name in sorted(set(ratchets) - set(lines)):
        print(f"build-budget: note: ratchet for {name} names no workspace lib; remove it.")
    return errors


def check_line_target(name, count, target):
    if count > target * 3 // 4:
        print(f"build-budget: {name} lib {count} lines (target {target})")
    if count > target:
        return (f"{name} lib is {count} lines, above the {target}-line target. "
                "Split it into smaller crates.")
    return None


def ratchet_error(name, count, limit, context):
    if count <= limit:
        return None
    return (f"{name} lib is {count} lines, more than {limit} {context}. "
            "Move code out of the crate (or delete some) rather than raising its ceiling.")


def main():
    args = parse_args()
    if args is None:
        return 2
    budget = tomllib.loads(BUDGET.read_text())
    test_binaries, lines, unbuilt = measure()
    base = base_state(args.base)
    if base is None:
        print(f"build-budget: no {args.base}; checking the ceilings only")
        base = (None, {}, {})
    else:
        print(f"build-budget: comparing with {args.base}")

    errors = check_tests(test_binaries, unbuilt, budget["test_binaries"]["max"], base)
    errors.extend(check_libraries(lines, budget["lib_lines"], base, args.base))
    for error in errors:
        print(f"::error::build-budget: {error}" if os.environ.get("GITHUB_ACTIONS")
              else f"build-budget: error: {error}", file=sys.stderr)
    if errors:
        return 1
    print("build-budget: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
