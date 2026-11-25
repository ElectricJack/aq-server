#!/usr/bin/env python3
"""Validate the repository's planning DAG and optionally render its Markdown view.

This does not call an AQ daemon, create issues, or execute implementation tasks.
Set AQ_PLAN_PLATFORM_ROOT to a platform reference containing sibling checkouts
when the local clones do not contain the historical evidence documents.
"""

import argparse
import json
import os
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "docs/plans/0001-v2-milestones.json"
OUTPUT = SOURCE.with_suffix(".md")


def checkout_root(root):
    """Find the common checkout for sibling docs, including linked worktrees."""
    marker = root / ".git"
    if marker.is_file():
        git_dir = (root / marker.read_text().strip().removeprefix("gitdir: ")).resolve()
        common = git_dir / "commondir"
        if common.is_file():
            return (git_dir / common.read_text().strip()).resolve().parent
    return root


CHECKOUT_ROOT = checkout_root(ROOT)


def owner_directory(name):
    name = {"agent-q": "aq-server"}.get(name, name)
    platform_root = Path(os.environ.get("AQ_PLAN_PLATFORM_ROOT", CHECKOUT_ROOT.parent))
    return ROOT if name == "aq-server" else platform_root / name


def plan_path(relative):
    parts = Path(relative).parts
    if parts[:3] == ("..", "..", ".."):
        return owner_directory(parts[3]).joinpath(*parts[4:])
    return SOURCE.parent / relative


def require(condition, message):
    if not condition:
        raise ValueError(message)


def read_plan():
    plan = json.loads(SOURCE.read_text())
    require(plan["schema_version"] == 1, "unsupported plan schema")
    tasks = plan["tasks"]
    by_id = {task["id"]: task for task in tasks}
    require(len(by_id) == len(tasks), "duplicate task IDs")
    milestones = {item["id"]: item for item in plan["milestones"]}
    require(len(milestones) == len(plan["milestones"]), "duplicate milestone IDs")
    requirements = known_requirements(plan["requirement_sources"])
    covered = set()
    for task in tasks:
        validate_task(task, plan["status_values"], milestones)
        validate_dependencies(task, by_id, requirements)
        covered.update(task["requirements"])
    require(covered == requirements, f"unassigned requirements: {sorted(requirements - covered)}")

    order = dependency_order(by_id)
    check_gates(milestones, by_id, order, plan["milestones"][-1]["gate"])
    check_references(plan, by_id)
    return plan, order, requirements


def known_requirements(sources_by_prefix):
    requirements = set()
    for prefix, sources in sources_by_prefix.items():
        for relative in sources:
            path = plan_path(relative)
            require(path.is_file(), f"missing requirement source: {relative}")
            requirements.update(re.findall(r"^\| (" + prefix + r"\d+) \|", path.read_text(), re.M))
    return requirements


def validate_task(task, statuses, milestones):
    name = task["id"]
    require(re.fullmatch(r"[A-Z]+-\d{2}", name), f"invalid task ID: {name}")
    require(task["status"] in statuses, f"invalid status: {name}")
    require(task["milestone"] in milestones, f"invalid milestone: {name}")
    require(owner_directory(task["repo"]).is_dir(), f"missing owner repo/directory: {name}")
    require(task["scope"] and task["acceptance"], f"missing task contract: {name}")


def validate_dependencies(task, by_id, requirements):
    name = task["id"]
    deps = task["depends_on"]
    require(len(deps) == len(set(deps)), f"duplicate dependencies: {name}")
    require(name not in deps, f"self dependency: {name}")
    require(set(deps) <= by_id.keys(), f"unknown dependency: {name}")
    require(set(task["requirements"]) <= requirements, f"unknown requirement: {name}")
    if task["status"] == "complete":
        require(task["evidence"], f"completed task lacks evidence: {name}")
        require(all(by_id[d]["status"] == "complete" for d in deps), f"incomplete prerequisite: {name}")
    for relative in task["evidence"]:
        require(plan_path(relative).exists(), f"missing evidence for {name}: {relative}")


def dependency_order(by_id):
    visiting, visited, order = set(), set(), []

    def visit(name):
        require(name not in visiting, f"dependency cycle at {name}")
        if name in visited:
            return
        visiting.add(name)
        for dependency in by_id[name]["depends_on"]:
            visit(dependency)
        visiting.remove(name)
        visited.add(name)
        order.append(name)

    for name in by_id:
        visit(name)

    return order


def check_gates(milestones, by_id, order, final_gate):
    # Every direct prerequisite has already been visited in dependency order.
    reachable = {}
    for name in order:
        predecessors = {name}
        for dependency in by_id[name]["depends_on"]:
            predecessors.update(reachable[dependency])
        reachable[name] = predecessors
    for milestone in milestones.values():
        gate = milestone["gate"]
        require(gate in by_id, f"unknown milestone gate: {gate}")
        require(by_id[gate]["milestone"] == milestone["id"], f"gate in wrong milestone: {gate}")
        required = {task["id"] for task in by_id.values() if task["milestone"] == milestone["id"]}
        require(required <= reachable[gate], f"milestone tasks not covered by gate: {milestone['id']}")
    require(set(by_id) <= reachable[final_gate], "tasks disconnected from final gate")


def check_references(plan, by_id):
    for key in ("audit", "snapshot"):
        require((SOURCE.parent / plan[key]).is_file(), f"missing {key}")
    for update in plan.get("design_updates", []):
        require(plan_path(update["source"]).is_file(), "missing design update source")
        require(update["summary"], "missing design update summary")
    for reference in plan.get("implementation_reference_maps", []):
        check_inventory(reference, by_id)


def check_inventory(reference, by_id):
    inventory_path = SOURCE.parent / reference["inventory"]
    require((SOURCE.parent / reference["guide"]).is_file(), "missing implementation reference guide")
    inventory = json.loads(inventory_path.read_text())
    require(re.fullmatch(r"[0-9a-f]{40}", inventory["source_revision"]), "unpinned implementation reference")
    for entry in inventory["entries"]:
        check_reference_entry(entry, inventory["files"], by_id)


def check_reference_entry(entry, files, by_id):
    require(entry["tasks"] and set(entry["tasks"]) <= by_id.keys(), "unknown implementation reference task")
    require(entry["rationale"] and entry["coverage"], "missing reference disposition/coverage")
    require(entry["disposition"] in {"preserve", "adapt", "replace", "omit"}, "invalid reference disposition")
    for source in entry["sources"] + entry["tests"]:
        require(source["path"] in files, "unhashed implementation reference")
        require(source["sha256"] == files[source["path"]]["sha256"], "reference hash disagreement")
        require(re.fullmatch(r"[0-9a-f]{64}", source["sha256"]), "invalid reference hash")
        require(source["symbol"] and 0 < source["line"] <= source["end_line"], "invalid reference symbol")


def ready_tasks(plan):
    by_id = {task["id"]: task for task in plan["tasks"]}
    return [task for task in plan["tasks"] if task["status"] == "planned"
            and all(by_id[dep]["status"] == "complete" for dep in task["depends_on"])]


def link(name):
    return f"[{name}](#{name.lower()})"


def render(plan):
    lines = [
        "# Agent Q v2 implementation milestones", "",
        f"> **Frozen historical baseline:** {plan['freeze_note']} "
        f"Tracking authority: [{Path(plan['tracking_authority']).name}](../../{plan['tracking_authority']}).", "",
        f"**Planning baseline:** {plan['date']}", "",
        "This is the historical execution plan for the [implementation audit](" + plan["audit"] + "). "
        "The [JSON task graph](0001-v2-milestones.json) is the canonical source for this frozen snapshot. "
        "This Markdown view is generated with `python3 scripts/check-v2-plan.py --render`. "
        "Preserve historical task data; track current work in the authority above. "
        "See the [requirement sources and old-to-new mapping](requirement-sources.md).", "",
        plan["format_note"], "",
        "## How to execute this plan", "",
        "Start with the ready tasks below. Before claiming a task, compare the "
        "[audit snapshot](" + plan["snapshot"] + ") with current work and identify any overlapping implementation. "
        "Qualify existing component code rather than rebuilding it. Pin contract/candidate revisions in each task's evidence.", "",
        plan["dependency_semantics"], "",
        "Each task has one owning repository. Contract work is reviewed by both endpoints, while integration gates are owned by agent-q. "
        "When a task is too broad after investigation, split it into bounded child tasks with explicit dependencies and keep the original gate dependent on them. "
        "A failed benchmark or newly discovered capability gap adds work to the graph; it is not grounds to mark a gate complete.", "",
        "The original planning baseline completed AUD-00; current statuses below record subsequent implementation and qualification. Existing code and recorded tests are reusable evidence, "
        "not proof that the new product integration tasks have already passed. No elapsed-time estimates or staffing assumptions are encoded.", "",
        "### Definition of done", "",
    ]
    lines += ["- " + item for item in plan["definition_of_done"]]
    append_design_references(lines, plan)
    lines += ["", "### Ready work", "", "| Task | Owner | Deliverable |", "| --- | --- | --- |"]
    lines += [f"| {link(task['id'])} | {task['repo']} | {task['title']} |" for task in ready_tasks(plan)]
    lines += ["", "The daemon authority track can start after BASE-01 and CON-01. The worker runtime track follows connector extraction. "
              "Graph, Git and playbook qualification are independent early tracks; they are not artificial prerequisites for fake-agent execution. "
              "Dashboard work waits for a versioned inspection/action API, but does not wait for production deployment.", "",
              "## Milestone outcomes", "", "| Milestone | Acceptance gate | Observable result |", "| --- | --- | --- |"]
    append_milestone_tables(lines, plan)
    lines += ["## Task contracts", ""]
    for task in plan["tasks"]:
        append_contract(lines, task)
    lines += ["## Validation and updates", "", "```sh", "python3 scripts/check-v2-plan.py",
              "python3 scripts/check-v2-plan.py --ready", "python3 scripts/check-v2-plan.py --render", "```", "",
              "The checker verifies unique IDs, known requirements, evidence references, acyclicity, hard prerequisites for completed tasks, "
              "milestone gate coverage, final-gate reachability and generated-document consistency. It never creates live AQ tasks or starts implementation.", "",
              "When implementation work is admitted to AQ later, retain these stable plan IDs as provenance. "
              "Use one task-status authority; explicitly replace this file's planning statuses with links to that execution system rather than maintaining two independently writable queues.", ""]
    return "\n".join(lines)


def append_design_references(lines, plan):
    if plan.get("design_updates"):
        lines += ["", "### Design additions", ""]
        lines.extend(f"- [{Path(update['source']).name}]({update['source']}): {update['summary']}"
                     for update in plan["design_updates"])
    if plan.get("implementation_reference_maps"):
        lines += ["", "### V1 implementation evidence", "",
                  "Read the mapped source and regression cases before implementing an affected task. "
                  "The pinned references inform the v2 contract; inventory inclusion is not passing v2 coverage. "
                  "Normal builds and plan checks require no v1 checkout.", ""]
        lines.extend(f"- [Reference guide]({ref['guide']}) and [pinned symbol/test inventory]({ref['inventory']})."
                     for ref in plan["implementation_reference_maps"])


def append_milestone_tables(lines, plan):
    for milestone in plan["milestones"]:
        lines.append(f"| {milestone['id']}: {milestone['title']} | {link(milestone['gate'])} | {milestone['outcome']} |")
    lines += ["", "The milestone spine below is a summary. The task dependencies in the JSON are authoritative.", "",
              "```mermaid", "flowchart LR", '    M1[Contract and connector] --> M2[Fake-agent execution]',
              '    M2 --> M3[Recovery proof]', '    M3 --> M4[Real agent and retained source]',
              '    G[Graph and Git candidates] --> M4', '    M4 --> M5[Policy pilots]',
              '    P[Playbook candidate] --> M5', '    M5 --> M6[Product acceptance]',
              '    U[API and dashboard] --> M6', '    R[Manifest and upgrade work] --> M6', "```", "",
              "## Task graph", "", "Requirements name implementation responsibility, not passing test coverage. "
              "E refers to ecosystem acceptance; X to shared failure scenarios; D to daemon requirements; W to worker requirements; U to dashboard acceptance; "
              "P to profile/intelligence requirements; H to harness/host requirements; R to remote terminal requirements; "
              "S to session observability/recovery; N to human notifications; C to agent waits/continuations.", ""]
    for milestone in plan["milestones"]:
        lines += [f"### {milestone['id']} overview", "", "| Task | Owner | Hard prerequisites | Status |", "| --- | --- | --- | --- |"]
        for task in plan["tasks"]:
            if task["milestone"] != milestone["id"]:
                continue
            deps = ", ".join(link(dep) for dep in task["depends_on"]) or "None"
            lines.append(f"| {link(task['id'])}: {task['title']} | {task['repo']} | {deps} | {task['status']} |")
        lines.append("")


def append_contract(lines, task):
    deps = ", ".join(link(dep) for dep in task["depends_on"]) or "None"
    lines += [f"### {task['id']}", "", f"**{task['title']}**", "",
              f"Owner: `{task['repo']}`. Milestone: {task['milestone']}. Kind: {task['kind']}. Status: {task['status']}.", "",
              f"Hard prerequisites: {deps}.", "", task["scope"], "", "Acceptance:", ""]
    lines += ["- " + item for item in task["acceptance"]]
    lines += ["", "Requirement links: " + ", ".join(task["requirements"]) + ".", ""]
    if task["out_of_scope"]:
        lines += ["Outside this task: " + "; ".join(task["out_of_scope"]) + ".", ""]
    if task["evidence"]:
        lines += ["Evidence: " + ", ".join(f"[{Path(item).name}]({item})" for item in task["evidence"]) + ".", ""]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--render", action="store_true", help="regenerate the Markdown plan")
    parser.add_argument("--ready", action="store_true", help="print ready task IDs as JSON")
    args = parser.parse_args()
    plan, order, requirements = read_plan()
    rendered = render(plan)
    if args.render:
        OUTPUT.write_text(rendered)
    else:
        require(OUTPUT.exists() and OUTPUT.read_text() == rendered, "Markdown plan is stale; run --render")
    if args.ready:
        print(json.dumps([task["id"] for task in ready_tasks(plan)]))
    else:
        print(f"PASS: {len(order)} tasks, {len(plan['milestones'])} milestones, "
              f"{len(requirements)} assigned requirements/scenarios; DAG and Markdown are consistent.")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
