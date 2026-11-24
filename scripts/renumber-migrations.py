#!/usr/bin/env python3
"""Renumber the migrations a branch adds so they follow origin/main's newest one.

Two branches that each add migration N cannot both merge: the catalog
(`scripts/migration_catalog.py`) needs unique, gap-free numbers. Run this on
the branch (after `git fetch origin`) instead of renaming by hand:

  python3 scripts/renumber-migrations.py          # renumber, print every change
  python3 scripts/renumber-migrations.py --check  # report only; exit 1 if needed

The migrations the branch adds are the SQL files in the working tree that the
merge-base with origin/main lacks. In order, they become main's highest number
plus one, two, and so on. The number is also rewritten where the branch itself
wrote it, and only on lines the branch added or changed (numbers on main's own
lines belong to main's migrations):

  * the file name, and any `NNNN_description` file stem named in text;
  * `value='N'` (the migration's `UPDATE meta SET value='N'`);
  * identifiers ending in `_N`, for example `COUNTS_N` and `PRIORS_N`;
  * `supported >= N`, `STATE_FORMAT_VERSION < N` and `*_FORMAT N` comparisons;
  * a version constant named `FORMAT` or `*_FORMAT` (any case), as in
    `pub const FORMAT: u32 = N;`; other constants keep their numbers;
  * `migration N` in comments and docs, and a zero padded `NNNN`.

The catalog itself is generated from the directory, so it needs no edit. If
the branch has not merged origin/main yet, main's own new migrations are
missing from the tree; the script says so, and the catalog passes after the
merge. Files are renamed and edited in the working tree and not staged.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from migration_catalog import state_format  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS = "crates/aq-server/migrations"
NAME = re.compile(r"([0-9]{4})_([a-z0-9_]+)\.sql")


class Failure(Exception):
    pass


def git(root, *args):
    result = subprocess.run(
        ["git", "-c", "core.quotepath=false", *args], cwd=root,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if result.returncode:
        raise Failure(f"git {' '.join(args)}: {result.stderr.strip()}")
    return result.stdout


def numbered(names):
    found = {}
    for name in names:
        match = NAME.fullmatch(name)
        if match:
            found[name] = int(match[1])
    return found


def plan_migrations(root, base_ref):
    """The (old name, new name) pairs, in order, for the branch's added migrations."""
    merge_base = git(root, "merge-base", "HEAD", base_ref).strip()
    in_base = set(git(root, "ls-tree", "--name-only", merge_base, f"{MIGRATIONS}/").split("\n"))
    in_base = {Path(path).name for path in in_base if path}
    on_main = numbered(Path(path).name for path in
                       git(root, "ls-tree", "--name-only", base_ref, f"{MIGRATIONS}/").split("\n") if path)
    if not on_main:
        raise Failure(f"{base_ref} has no migrations")
    directory = root / MIGRATIONS
    added = sorted(name for name in numbered(os.listdir(directory)) if name not in in_base)
    top = max(on_main.values())
    pairs = []
    for offset, old in enumerate(added, 1):
        pairs.append((old, f"{top + offset:04}_{NAME.fullmatch(old)[2]}.sql"))
    return merge_base, pairs, top, on_main


def added_lines(root, merge_base):
    """{path: set of 1-based line numbers the working tree adds to the merge-base}, None for all."""
    diff = git(root, "diff", "-U0", "--no-color", "--no-ext-diff", "--no-renames", merge_base)
    lines = changed_line_numbers(diff)
    untracked = git(root, "ls-files", "--others", "--exclude-standard").split("\n")
    lines.update(dict.fromkeys(filter(None, untracked)))
    return lines


def changed_line_numbers(diff):
    lines = {}
    current = None
    for line in diff.split("\n"):
        if line == "+++ /dev/null":
            current = None
        elif line.startswith("+++ "):
            path = line[6:].split("\t")[0]
            current = lines.setdefault(path, set())
        elif current is not None and line.startswith("@@"):
            current.update(hunk_lines(line))
    return lines


def hunk_lines(header):
    match = re.match(r"@@ -\S+ \+(\d+)(?:,(\d+))? @@", header)
    start = int(match[1])
    return range(start, start + int(match[2] or "1"))


def number_text(match, new):
    """The new number written the way the old one was: zero padded to four, or plain."""
    return f"{new:04}" if len(match) >= 4 and match.startswith("0") else str(new)


def rewriter(numbers, stems):
    """A function rewriting one line, every old number at once (so 154->155, 155->156 is safe)."""
    stem_alt = "|".join(re.escape(stem) for stem in sorted(stems, key=len, reverse=True))
    patterns = [
        (r"(?<![0-9A-Za-z_])(?P<stem>" + stem_alt + r")(?![A-Za-z0-9_])") if stems else r"(?!)",
        r"(?P<ident>(?<![A-Za-z0-9_])[A-Za-z][A-Za-z0-9]*(?:_[A-Za-z0-9]+)*_)(?P<n1>[0-9]+)(?![A-Za-z0-9_])",
        r"(?P<gate>(?<![A-Za-z0-9_])[A-Za-z_]*(?i:supported|format|version)[A-Za-z_]*\s*"
        r"(?:>=|<=|==|!=|>|<|=|:)\s*)(?P<n2>[0-9]+)(?![0-9A-Za-z_])(?!\.[0-9])",
        r"(?P<fmt>(?<![A-Za-z0-9_])[A-Za-z0-9_]*(?i:format)[A-Za-z0-9_]*[ \t]+)(?P<n3>[0-9]+)(?![0-9A-Za-z_])(?!\.[0-9])",
        r"(?P<const>(?<![A-Za-z0-9_])(?:pub(?:\([a-z: ]+\))?\s+)?const\s+(?:[A-Za-z0-9_]*_)?(?i:format)\s*:\s*"
        r"(?:[iu](?:8|16|32|64|128|size))\s*=\s*)(?P<n7>[0-9]+)(?![0-9A-Za-z_])(?!\.[0-9])",
        r"(?P<sql>value\s*=\s*')(?P<n4>[0-9]+)(?=')",
        r"(?P<mig>(?i:\bmigrations?\s+(?:no\.?\s*)?))(?P<n5>[0-9]+)(?![0-9A-Za-z_])",
        r"(?<![0-9A-Za-z_])(?<!\.)(?P<n6>0[0-9]{3})(?![0-9A-Za-z_])(?!\.[0-9])",
    ]
    combined = re.compile("|".join(f"(?:{pattern})" for pattern in patterns))

    def replace(match):
        if match["stem"]:
            return stems[match["stem"]]
        for prefix, group in [("ident", "n1"), ("gate", "n2"), ("fmt", "n3"), ("const", "n7"), ("sql", "n4"),
                              ("mig", "n5"), (None, "n6")]:
            digits = match[group]
            if digits is not None:
                new = numbers.get(int(digits))
                if new is None:
                    return match[0]
                return (match[prefix] if prefix else "") + number_text(digits, new)
        return match[0]

    return lambda line: combined.sub(replace, line)


def plan(root, base_ref):
    merge_base, pairs, top, on_main = plan_migrations(root, base_ref)
    renames = [(old, new) for old, new in pairs if old != new]
    edits = {}
    if renames:
        numbers = {int(old[:4]): int(new[:4]) for old, new in renames}
        stems = {old[:-4]: new[:-4] for old, new in renames}
        added = {f"{MIGRATIONS}/{old}" for old, _ in pairs}
        edits = plan_edits(root, added, added_lines(root, merge_base), rewriter(numbers, stems))
    return pairs, renames, edits, top, on_main


def plan_edits(root, added, changed, rewrite):
    edits = {}
    # Every line in a new migration belongs to the branch. Other migrations belong to main.
    candidates = {path: which for path, which in changed.items() if not path.startswith(f"{MIGRATIONS}/")}
    candidates.update(dict.fromkeys(added))
    for path, which in sorted(candidates.items()):
        text = read_candidate(root / path)
        if text is None:
            continue
        new_text = rewrite_lines(text, which, rewrite)
        if new_text != text or path in added:
            edits[path] = new_text
    return edits


def read_candidate(path):
    if path.is_file():
        try:
            return path.read_bytes().decode()
        except UnicodeDecodeError:
            return None
    return None


def rewrite_lines(text, which, rewrite):
    lines = text.splitlines(keepends=True)
    if which is None:
        return "".join(map(rewrite, lines))
    return "".join(rewrite(line) if i in which else line for i, line in enumerate(lines, 1))


def run(root, base_ref, check):
    pairs, renames, edits, top, on_main = plan(root, base_ref)
    if not renames:
        print(f"no renumbering needed: {len(pairs)} new migration(s) already follow "
              f"{base_ref}'s newest ({top:04})")
        return 0
    print(f"renumbering needed: {base_ref}'s newest migration is {top:04}")
    report_changes(renames, edits, "would" if check else "did")
    if check:
        return 1
    apply_changes(root, renames, edits)
    check_catalog(root, base_ref, on_main)
    return 0


def report_changes(renames, edits, verb):
    moved = dict(renames)
    for old, new in renames:
        print(f"  {verb} rename {MIGRATIONS}/{old} -> {new}")
    for path in sorted(edits):
        if Path(path).name not in moved:
            print(f"  {verb} update {path}")


def apply_changes(root, renames, edits):
    directory = root / MIGRATIONS
    moved = dict(renames)
    for old, _ in renames:
        (directory / old).unlink()
    for path, text in edits.items():
        name = Path(path).name
        if path == f"{MIGRATIONS}/{name}" and name in moved:
            path = f"{MIGRATIONS}/{moved[name]}"
        (root / path).write_text(text)


def check_catalog(root, base_ref, on_main):
    names = set(os.listdir(root / MIGRATIONS))
    missing = sorted(set(on_main) - names)
    try:
        print(f"migration catalog: state format {state_format(root)}")
    except ValueError as error:
        if missing:
            print(f"note: this tree lacks {len(missing)} migration(s) of {base_ref} "
                  f"(first {missing[0]}); merge {base_ref}, then run "
                  "python3 scripts/migration_catalog.py")
        else:
            raise Failure(f"migration catalog fails after renumbering: {error}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--base", default="origin/main", help="the branch to follow (default origin/main)")
    parser.add_argument("--check", action="store_true",
                        help="only report; exit 1 when renumbering is needed")
    args = parser.parse_args()
    try:
        return run(args.root.resolve(), args.base, args.check)
    except Failure as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
