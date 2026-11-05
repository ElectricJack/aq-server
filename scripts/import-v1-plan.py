#!/usr/bin/env python3
"""Import the v2 program plan's task file into an AQ v1 project.

Usage:
    python3 scripts/import-v1-plan.py --tasks docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml \
        --project aq-platform --mapping docs/superpowers/plans/2026-11-02-v2-full-application-v1-ids.json \
        [--release-gate MS0] [--dry-run]

Behaviour (plan §4.3):
  * validates the whole file first and creates nothing if any check fails;
  * creates one container per epic, then each non-deferred task with its workspace kind,
    type, intelligence-class hint, integration mode and deliverables, then the standard subtasks;
  * adds every `needs` edge only after all tasks exist;
  * with --release-gate KEY, every active root task (no needs) except KEY's own prerequisites
    additionally waits for KEY, so the gate's milestone runs first; those edges are recorded
    as `release_gate_edges` in the mapping so they can be removed when the gate is approved;
  * is idempotent: the mapping file (key -> v1 id) is rewritten after every creation, and keys
    already present are skipped on a rerun. Edges are re-added harmlessly only if missing.

Refuses to create anything unless the target project is PAUSED.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import yaml

REPO_KIND = {
    "aq-server": "project-repo",
    "aq-worker": "aq-worker-repo",
    "aq-dashboard": "aq-dashboard-repo",
    "aq-graph": "aq-graph-repo",
    "aq-git": "aq-git-repo",
    "aq-playbooks": "aq-playbooks-repo",
}
DELIVERABLE_KINDS = {"file", "test", "review"}
# v1 schedules lower numbers first; 100 is the default.
PRIORITY = {"high": 50, "normal": 100, "low": 200}
REVIEW_TARGETS = {"spec", "plan", "other", "any"}
PLAN_DOC = "aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-plan.md"
TASK_FILE = "aq-server/docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml"
PLATFORM_REF = "/home/jkern/dev/aq-platform"

FOOTER = """\
---
**This description is your complete scope.** Everything you need is above: goal, what to build, current state, interfaces and out-of-scope items. Do not read whole design documents. Open a listed reference section only to settle a specific ambiguity. Never implement work listed under Out of scope; the named task owns it.
**Complete this whole task in this session.** Don't stop after one sub-step. Follow the plan's Global Constraints ({plan}).
Write this task's TDD implementation plan (superpowers:writing-plans) in the owning repository before writing code. Then implement it test-first.
If you find work outside this task's scope in **this** repository, file it with `aq task create --integration-mode direct --reason ...` (direct: it lands on main without waiting for a PR review), then continue with this task. If the follow-up belongs to **another** repository, do not file it as a task (it would land in this repository's project). Instead add a task comment that starts with `CROSS-REPO FOLLOW-UP (<repo>):` followed by a complete, self-contained description, then continue; the operator files it in the right project, and it does not block your close unless your acceptance needs it. Don't edit `aq-server/docs/architecture/*` or `docs/specs/*` unless this is an A-series or docs task. Record proposed design-doc changes in your evidence and file a `docs` task.
If a decision only the owner can make blocks you, report it with `aq message send --to user:dashboard --project "$AQ_PROJECT_ID" --body "Blocked on {key} (<task id>): <the decision needed>"`, record it as a task comment, and wait for the reply. Don't guess.
Each named-test deliverable must exist and pass. A filter that matches zero tests is not met.
Close `pass` only when every acceptance item is met with evidence in `docs/implementation/{key}.md` in the owning repository. Otherwise close `fail` with what remains.
"""

SUBTASKS = [
    "Read this description and the current code it names; open a reference section only if something is ambiguous. Record the findings as a task comment.",
    "Write and commit this task's implementation plan (superpowers:writing-plans).",
    "Implement test-first. The named tests in the acceptance criteria must exist and pass.",
    "Run the full definition-of-done checks for every touched repository.",
    "Write the evidence document: commands, environment, commit, limits.",
    "Get an adversarial review if this task is marked for one, and resolve the findings.",
    "Commit, push and close with --changes, --verification and --test.",
]


def fail(msg: str) -> None:
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(2)


def validate(doc: dict) -> tuple[dict, list[dict], list[dict]]:
    epics = {e["key"]: e for e in doc["epics"]}
    tasks = doc["tasks"]
    by_key: dict[str, dict] = {}
    errors: list[str] = []
    for t in tasks:
        k = t.get("key")
        if not k or k in by_key:
            errors.append(f"missing or duplicate key: {k!r}")
            continue
        by_key[k] = t
    for k, t in by_key.items():
        for field in ("epic", "repo", "title", "type", "description", "acceptance", "deliverables"):
            if not t.get(field):
                errors.append(f"{k}: missing {field}")
        if t.get("epic") not in epics:
            errors.append(f"{k}: unknown epic {t.get('epic')!r}")
        if t.get("repo") not in REPO_KIND:
            errors.append(f"{k}: repo {t.get('repo')!r} has no workspace kind")
        for n in t.get("needs", []):
            if n not in by_key:
                errors.append(f"{k}: needs unknown {n}")
            elif not t.get("deferred") and by_key[n].get("deferred"):
                errors.append(f"{k}: active task needs deferred {n}")
        for d in t.get("deliverables", []):
            if not all(d.get(x) for x in ("id", "kind", "target")):
                errors.append(f"{k}: malformed deliverable {d}")
            elif d["kind"] not in DELIVERABLE_KINDS:
                errors.append(f"{k}: deliverable kind {d['kind']!r}")
            elif d["kind"] == "review" and d["target"] not in REVIEW_TARGETS:
                errors.append(f"{k}: review target {d['target']!r}")
            elif d["kind"] == "test" and d["target"].strip() in {"cargo test --locked --workspace", "pnpm test"} \
                    and not k.startswith("A") and not t.get("deferred"):
                errors.append(f"{k}: generic test deliverable (use a named test)")
    # cycle check
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
    if errors:
        fail("validation failed:\n  " + "\n  ".join(errors))
    active = [t for t in tasks if not t.get("deferred")]
    deferred = [t for t in tasks if t.get("deferred")]
    return epics, active, deferred


def description(t: dict) -> str:
    parts = [t["description"].strip()]
    parts.append("**Acceptance**\n" + "\n".join(f"- {a}" for a in t["acceptance"]))
    if t.get("spec") and "### References" not in t["description"]:
        parts.append("**References** (consult only if something above is ambiguous; paths relative to platform-ref at "
                     + PLATFORM_REF + ")\n" + "\n".join(f"- {s}" for s in t["spec"]))
    if t.get("continues"):
        parts.append("**Continues 2026-10-30 plan tasks:** " + ", ".join(t["continues"]))
    meta = [f"plan key {t['key']}", f"repo {t['repo']}"]
    if t.get("review"):
        meta.append(f"review: {t['review']}")
    parts.append("**Plan:** " + "; ".join(meta) + f". Task contract: {TASK_FILE}.")
    parts.append(FOOTER.format(plan=PLAN_DOC, key=t["key"]))
    return "\n\n".join(parts)


class Aq:
    def __init__(self, dry: bool):
        self.dry = dry
        self.counter = 0

    def run(self, *args: str) -> dict:
        cmd = ["aq", "--json", *args]
        if self.dry:
            self.counter += 1
            return {"created": f"DRY-{self.counter}"}
        res = subprocess.run(cmd, capture_output=True, text=True)
        try:
            env = json.loads(res.stdout)
        except json.JSONDecodeError:
            fail(f"non-JSON output from {' '.join(cmd[:6])}...: {res.stdout[-500:]} {res.stderr[-500:]}")
        if env.get("error") or res.returncode != 0:
            fail(f"{' '.join(cmd[:6])}... failed: {env.get('error') or res.stderr[-500:]}")
        return env.get("data") or {}


def project_paused(project: str) -> bool:
    res = subprocess.run(["aq", "--json", "project", "list"], capture_output=True, text=True)
    data = json.loads(res.stdout)["data"]
    projects = data if isinstance(data, list) else data.get("projects", [])
    for p in projects:
        if p.get("id") == project:
            return str(p.get("status", "")).upper() == "PAUSED"
    fail(f"project {project} not found")
    return False


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--project", required=True)
    ap.add_argument("--mapping", required=True)
    ap.add_argument("--release-gate")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()

    doc = yaml.safe_load(Path(a.tasks).read_text())
    epics, active, deferred = validate(doc)
    active_keys = {t["key"] for t in active}
    if a.release_gate and a.release_gate not in active_keys:
        fail(f"release gate {a.release_gate} is not an active task")

    mapping_path = Path(a.mapping)
    mapping = json.loads(mapping_path.read_text()) if mapping_path.exists() else {}
    mapping.setdefault("project", a.project)
    mapping.setdefault("epics", {})
    mapping.setdefault("tasks", {})
    mapping.setdefault("edges", [])
    mapping.setdefault("release_gate_edges", [])

    # Edge plan.
    edges = [(t["key"], n) for t in active for n in t.get("needs", [])]
    gate_edges: list[tuple[str, str]] = []
    if a.release_gate:
        gate = next(t for t in active if t["key"] == a.release_gate)
        exempt = {a.release_gate}
        stack = list(gate.get("needs", []))
        while stack:  # the gate's whole prerequisite closure is exempt
            k = stack.pop()
            if k not in exempt:
                exempt.add(k)
                stack += next(t for t in active if t["key"] == k).get("needs", [])
        gate_edges = [(t["key"], a.release_gate) for t in active
                      if not t.get("needs") and t["key"] not in exempt]

    per_repo: dict[str, int] = {}
    for t in active:
        per_repo[t["repo"]] = per_repo.get(t["repo"], 0) + 1
    print(f"Project {a.project}: {len(epics)} epics, {len(active)} active tasks, {len(deferred)} deferred (skipped)")
    print("  by repo: " + ", ".join(f"{r} {n}" for r, n in sorted(per_repo.items())))
    print(f"  needs edges: {len(edges)}; release-gate edges onto {a.release_gate}: {len(gate_edges)}")
    if gate_edges:
        print("    gated roots: " + ", ".join(k for k, _ in gate_edges))
    print(f"  deliverables: {sum(len(t['deliverables']) for t in active)}; "
          f"pull_request tasks: {sum(1 for t in active if t.get('integration') == 'pull_request')}; "
          f"human-verified gates: {sum(1 for t in active if t.get('verification') == 'human')}")
    already = [t["key"] for t in active if t["key"] in mapping["tasks"]]
    if already:
        print(f"  already imported (will skip): {len(already)}")
    ready_now = [t["key"] for t in active if not t.get("needs") and t["key"] not in {k for k, _ in gate_edges}]
    print(f"  ready once the project resumes: {', '.join(ready_now)}")
    print("  deferred: " + ", ".join(t["key"] for t in deferred))

    if not a.dry_run and not project_paused(a.project):
        fail(f"project {a.project} must be PAUSED before import (aq project pause --project-id {a.project})")

    aq = Aq(a.dry_run)

    def save() -> None:
        if not a.dry_run:
            mapping_path.write_text(json.dumps(mapping, indent=2) + "\n")

    used_epics = {t["epic"] for t in active}
    for key, e in epics.items():
        if key not in used_epics or key in mapping["epics"]:
            continue
        data = aq.run("task", "create", "-p", a.project, "-t", f"{key}. {e['title']}", "--container",
                      "-d", f"Workstream {key} of the AQ v2 program plan ({PLAN_DOC}).")
        mapping["epics"][key] = data["created"]
        save()

    for t in active:
        if t["key"] in mapping["tasks"]:
            continue
        args = ["task", "create", "-p", a.project, "-t", f"{t['key']}: {t['title']}",
                "-d", description(t), "--type", t["type"], "--parent", mapping["epics"].get(t["epic"], "DRY-epic"),
                "--requires-kind", REPO_KIND[t["repo"]]]
        if t.get("class"):
            args += ["--intelligence-class", t["class"]]
        args += ["--integration-mode", t.get("integration", "direct")]
        if t.get("priority") in PRIORITY:
            args += ["--priority", str(PRIORITY[t["priority"]])]
        for d in t["deliverables"]:
            args += ["--deliverable", json.dumps({"id": d["id"], "kind": d["kind"], "target": d["target"]})]
        data = aq.run(*args)
        tid = data["created"]
        mapping["tasks"][t["key"]] = tid
        save()
        if t.get("verification") == "human":
            aq.run("task", "edit", "--task-id", tid, "--verification-type", "human")
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
            json.dump([{"title": s} for s in SUBTASKS], f)
        aq.run("task", "subtask-add", tid, "--from-file", f.name)
        Path(f.name).unlink(missing_ok=True)
        print(f"  created {t['key']} -> {tid}")

    done = {tuple(e) for e in mapping["edges"]} | {tuple(e) for e in mapping["release_gate_edges"]}
    for (down, up), bucket, reason in (
        [(e, "edges", "plan dependency") for e in edges]
        + [(e, "release_gate_edges", f"release gate: wait for {a.release_gate} approval") for e in gate_edges]
    ):
        if (down, up) in done:
            continue
        aq.run("task", "add-dependency", "--task-id", mapping["tasks"].get(down, down),
               "--depends-on", mapping["tasks"].get(up, up), "--dep-type", "blocks", "--reason", reason)
        mapping[bucket].append([down, up])
        save()

    print("dry run: nothing created" if a.dry_run else f"import complete; mapping in {mapping_path}")


if __name__ == "__main__":
    main()
