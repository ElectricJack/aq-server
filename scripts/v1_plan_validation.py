"""Validate the task and dependency contracts for an AQ v1 plan import."""
from __future__ import annotations

import sys


REPO_KIND = {
    "aq-server": "project-repo",
    "aq-worker": "aq-worker-repo",
    "aq-dashboard": "aq-dashboard-repo",
    "aq-graph": "aq-graph-repo",
    "aq-git": "aq-git-repo",
    "aq-playbooks": "aq-playbooks-repo",
}
DELIVERABLE_KINDS = {"file", "test", "review"}
REVIEW_TARGETS = {"spec", "plan", "other", "any"}


def fail(msg: str) -> None:
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(2)


def validate(doc: dict) -> tuple[dict, list[dict], list[dict]]:
    epics = {e["key"]: e for e in doc["epics"]}
    tasks = doc["tasks"]
    errors = []
    by_key = task_index(tasks, errors)
    for key, task in by_key.items():
        validate_task(key, task, epics, errors)
        validate_dependencies(key, task, by_key, errors)
        validate_deliverables(key, task, errors)
    check_cycles(by_key, errors)
    if errors:
        fail("validation failed:\n  " + "\n  ".join(errors))
    active, deferred = partition_tasks(tasks)
    return epics, active, deferred


def partition_tasks(tasks):
    active, deferred = [], []
    for task in tasks:
        selected = deferred if task.get("deferred") else active
        selected.append(task)
    return active, deferred


def task_index(tasks, errors):
    by_key = {}
    for task in tasks:
        key = task.get("key")
        if not key or key in by_key:
            errors.append(f"missing or duplicate key: {key!r}")
        else:
            by_key[key] = task
    return by_key


def validate_task(k, t, epics, errors):
    for field in ("epic", "repo", "title", "type", "description", "acceptance", "deliverables"):
        if not t.get(field):
            errors.append(f"{k}: missing {field}")
    if t.get("epic") not in epics:
        errors.append(f"{k}: unknown epic {t.get('epic')!r}")
    if t.get("repo") not in REPO_KIND:
        errors.append(f"{k}: repo {t.get('repo')!r} has no workspace kind")


def validate_dependencies(k, t, by_key, errors):
    for n in t.get("needs", []):
        if n not in by_key:
            errors.append(f"{k}: needs unknown {n}")
        elif not t.get("deferred") and by_key[n].get("deferred"):
            errors.append(f"{k}: active task needs deferred {n}")


def validate_deliverables(key, task, errors):
    for item in task.get("deliverables", []):
        validate_deliverable(key, task, item, errors)


def validate_deliverable(k, t, d, errors):
    if not all(map(d.get, ("id", "kind", "target"))):
        errors.append(f"{k}: malformed deliverable {d}")
    elif d["kind"] not in DELIVERABLE_KINDS:
        errors.append(f"{k}: deliverable kind {d['kind']!r}")
    elif d["kind"] == "review" and d["target"] not in REVIEW_TARGETS:
        errors.append(f"{k}: review target {d['target']!r}")
    elif is_generic_test(d) and requires_named_test(k, t):
        errors.append(f"{k}: generic test deliverable (use a named test)")


def requires_named_test(key, task):
    return not key.startswith("A") and not task.get("deferred")


def is_generic_test(deliverable):
    return deliverable["kind"] == "test" and deliverable["target"].strip() in {"cargo test --locked --workspace", "pnpm test"}


def check_cycles(by_key, errors):
    state: dict[str, int] = {}

    def visit(k: str, path: tuple[str, ...]) -> None:
        if state.get(k) == 2:
            return
        if state.get(k) == 1:
            errors.append("cycle: " + " -> ".join(path + (k,)))
            return
        state[k] = 1
        for n in by_key[k].get("needs", []):
            if n in by_key:
                visit(n, path + (k,))
        state[k] = 2

    for k in by_key:
        visit(k, ())
