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
from collections import Counter
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import yaml

from v1_plan_validation import REPO_KIND, fail, validate


# v1 schedules lower numbers first; 100 is the default.
PRIORITY = {"high": 50, "normal": 100, "low": 200}
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
    # The plan predates per-repository projects. Route its owner questions to the worker's project.
    return "\n\n".join(parts).replace(
        "aq message send --to user:dashboard --project aq-platform`",
        'aq message send --to user:dashboard --project "$AQ_PROJECT_ID"`',
    )


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


def gate_dependencies(active, gate_key):
    if not gate_key:
        return []
    tasks = {task["key"]: task for task in active}
    if gate_key not in tasks:
        fail(f"release gate {gate_key} is not an active task")
    exempt = {gate_key}
    stack = list(tasks[gate_key].get("needs", []))
    while stack:
        key = stack.pop()
        if key not in exempt:
            exempt.add(key)
            stack.extend(tasks[key].get("needs", []))
    return [(t["key"], gate_key) for t in active if not t.get("needs") and t["key"] not in exempt]


def load_mapping(mapping_path, project):
    mapping = json.loads(mapping_path.read_text()) if mapping_path.exists() else {}
    mapping.setdefault("project", project)
    mapping.setdefault("epics", {})
    mapping.setdefault("tasks", {})
    mapping.setdefault("edges", [])
    mapping.setdefault("release_gate_edges", [])

    return mapping


def report_plan(a, tasks, dependencies, mapping):
    epics, active, deferred = tasks
    edges, gate_edges = dependencies
    per_repo = Counter(t["repo"] for t in active)
    print(f"Project {a.project}: {len(epics)} epics, {len(active)} active tasks, {len(deferred)} deferred (skipped)")
    print("  by repo: " + ", ".join(f"{r} {n}" for r, n in sorted(per_repo.items())))
    print(f"  needs edges: {len(edges)}; release-gate edges onto {a.release_gate}: {len(gate_edges)}")
    if gate_edges:
        print("    gated roots: " + ", ".join(k for k, _ in gate_edges))
    report_deliverables(active)
    report_roots(active, gate_edges, mapping)
    print("  deferred: " + ", ".join(t["key"] for t in deferred))


def report_deliverables(active):
    print(f"  deliverables: {sum(len(t['deliverables']) for t in active)}; "
          f"pull_request tasks: {sum(1 for t in active if t.get('integration') == 'pull_request')}; "
          f"human-verified gates: {sum(1 for t in active if t.get('verification') == 'human')}")


def report_roots(active, gate_edges, mapping):
    already = [t["key"] for t in active if t["key"] in mapping["tasks"]]
    if already:
        print(f"  already imported (will skip): {len(already)}")
    gated = {key for key, _ in gate_edges}
    ready_now = [t["key"] for t in active if not t.get("needs") and t["key"] not in gated]
    print(f"  ready once the project resumes: {', '.join(ready_now)}")


def deliverable_args(deliverables):
    for item in deliverables:
        yield "--deliverable"
        yield json.dumps({key: item[key] for key in ("id", "kind", "target")})


def task_args(project, parent, t):
    args = ["task", "create", "-p", project, "-t", f"{t['key']}: {t['title']}",
            "-d", description(t), "--type", t["type"], "--parent", parent,
            "--requires-kind", REPO_KIND[t["repo"]]]
    if t.get("class"):
        args += ["--intelligence-class", t["class"]]
    args += ["--integration-mode", t.get("integration", "direct")]
    if t.get("priority") in PRIORITY:
        args += ["--priority", str(PRIORITY[t["priority"]])]
    args.extend(deliverable_args(t["deliverables"]))
    return args


class PlanImport:
    def __init__(self, project, aq, mapping, mapping_path):
        self.project = project
        self.aq = aq
        self.mapping = mapping
        self.mapping_path = mapping_path

    def save(self):
        if not self.aq.dry:
            self.mapping_path.write_text(json.dumps(self.mapping, indent=2) + "\n")

    def create_epics(self, epics, active):
        used_epics = {t["epic"] for t in active}
        for key, e in epics.items():
            if key not in used_epics or key in self.mapping["epics"]:
                continue
            data = self.aq.run("task", "create", "-p", self.project, "-t", f"{key}. {e['title']}", "--container",
                          "-d", f"Workstream {key} of the AQ v2 program plan ({PLAN_DOC}).")
            self.mapping["epics"][key] = data["created"]
            self.save()

    def create_task(self, task):
        key = task["key"]
        if key in self.mapping["tasks"]:
            return
        parent = self.mapping["epics"].get(task["epic"], "DRY-epic")
        data = self.aq.run(*task_args(self.project, parent, task))
        tid = data["created"]
        self.mapping["tasks"][key] = tid
        self.save()
        if task.get("verification") == "human":
            self.aq.run("task", "edit", "--task-id", tid, "--verification-type", "human")
        self.create_subtasks(tid)
        print(f"  created {key} -> {tid}")

    def create_subtasks(self, tid):
        with tempfile.TemporaryDirectory() as directory:
            subtasks = Path(directory) / "subtasks.json"
            subtasks.write_text(json.dumps([{"title": s} for s in SUBTASKS]))
            self.aq.run("task", "subtask-add", tid, "--from-file", str(subtasks))

    def add_edges(self, edges, gate_edges, gate_key):
        done = set(map(tuple, self.mapping["edges"] + self.mapping["release_gate_edges"]))
        for (down, up), bucket, reason in (
            [(e, "edges", "plan dependency") for e in edges]
            + [(e, "release_gate_edges", f"release gate: wait for {gate_key} approval") for e in gate_edges]
        ):
            if (down, up) in done:
                continue
            self.aq.run("task", "add-dependency", "--task-id", self.mapping["tasks"].get(down, down),
                   "--depends-on", self.mapping["tasks"].get(up, up), "--dep-type", "blocks", "--reason", reason)
            self.mapping[bucket].append([down, up])
            self.save()

def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--project", required=True)
    ap.add_argument("--mapping", required=True)
    ap.add_argument("--release-gate")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()

    doc = yaml.safe_load(Path(a.tasks).read_text())
    tasks = validate(doc)
    epics, active, _ = tasks
    gate_edges = gate_dependencies(active, a.release_gate)
    edges = [(t["key"], n) for t in active for n in t.get("needs", [])]
    mapping_path = Path(a.mapping)
    mapping = load_mapping(mapping_path, a.project)
    report_plan(a, tasks, (edges, gate_edges), mapping)
    if not a.dry_run and not project_paused(a.project):
        fail(f"project {a.project} must be PAUSED before import (aq project pause --project-id {a.project})")

    importer = PlanImport(a.project, Aq(a.dry_run), mapping, mapping_path)
    importer.create_epics(epics, active)
    for task in active:
        importer.create_task(task)
    importer.add_edges(edges, gate_edges, a.release_gate)
    print("dry run: nothing created" if a.dry_run else f"import complete; mapping in {mapping_path}")


if __name__ == "__main__":
    main()
