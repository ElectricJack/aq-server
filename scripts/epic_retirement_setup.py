"""Plan the ordered Graph changeset that creates or adopts retirement tasks."""

from dataclasses import dataclass

from epic_retirement_graph import Graph
from epic_retirement_records import (
    META,
    create,
    link,
    patch,
    require,
    task_evidence,
    verify,
)


ROLES = (
    ("approval", "Approve integrated epic feature"),
    ("cleanup", "Remove the epic feature flags"),
    ("closure_gate", "Verify epic retirement evidence"),
)


@dataclass
class RetirementSetup:
    graph: Graph
    owner: str
    registration: str
    spec: dict

    def plan(self):
        epic = self.graph.node(self.spec["epic_id"], kind="task")
        require(epic["task_state"] in ("draft", "active"), "Epic is closed")
        approval, cleanup, closure = (
            self.spec[key] for key in ("approval", "cleanup", "closure_gate")
        )
        self.operations = [patch(epic)]
        self.versions = {}
        for key, title in ROLES:
            self.prepare_task(key, title)
        if self.spec["adopt"]:
            self.check_approval_edges(approval, cleanup)
        self.ensure_edge("blocks", approval, cleanup)
        self.ensure_edge("parent-child", epic["id"], cleanup)
        self.ensure_edge("parent-child", epic["id"], closure)
        self.operations += [
            verify(task, self.versions[task], [task_evidence(epic["id"], "epic")])
            for task in (approval, cleanup, closure)
        ]
        return self.operations

    def check_approval_edges(self, approval, cleanup):
        attached = self.graph.edges(approval)
        require(
            all(
                kind != "blocks" or (source == approval and target == cleanup)
                for kind, source, target in attached
            ),
            "Adopted approval has dependencies beyond this cleanup",
        )

    def prepare_task(self, key, title):
        task_id = self.spec[key]
        existing = self.graph.node(task_id, missing=True, kind="task")
        marker = {"registration": self.registration, "role": key}
        if self.spec["adopt"] and key != "closure_gate":
            self.adopt_task(existing, marker)
        else:
            require(
                existing is None,
                f"Retirement task {task_id} exists without a setup receipt",
            )
            self.create_task(marker, title)

    def adopt_task(self, existing, marker):
        key = marker["role"]
        require(
            existing
            and existing["task_state"] == "draft"
            and existing["created_by"] == self.owner,
            f"Adopt only an owner-created draft {key}",
        )
        metadata = existing["metadata"]
        require(
            META not in metadata or metadata[META] == marker,
            f"{existing['id']} is marked for a different retirement",
        )
        self.operations.append(patch(existing, metadata={**metadata, META: marker}))
        self.versions[existing["id"]] = existing["version"] + int(META not in metadata)

    def create_task(self, marker, title):
        key = marker["role"]
        task_id = self.spec[key]
        body = {
            "approval": "Owner gate. Record the vetted revisions and integrated test evidence through the retirement workflow.",
            "cleanup": f"{self.spec['permanent_behavior']} Record cleanup merges, ordinary builds and the integrated acceptance check.",
            "closure_gate": "Owner gate. The retirement workflow closes this task after validating all cleanup evidence.",
        }[key]
        self.operations.append(
            create(
                task_id,
                "task",
                title,
                task_state="draft",
                body=body,
                metadata={META: marker},
            )
        )
        self.versions[task_id] = 1

    def ensure_edge(self, kind, source, target):
        current = (
            self.graph.edges(target)
            if self.spec["adopt"]
            and target in (self.spec["approval"], self.spec["cleanup"])
            else []
        )
        same_kind = [edge for edge in current if edge[0] == kind and edge[2] == target]
        require(
            not same_kind or same_kind == [(kind, source, target)],
            f"Unexpected {kind} dependencies for {target}",
        )
        if not same_kind:
            self.operations.append(link(self.registration, kind, source, target))
            self.versions[target] += 1
            if source in self.versions:
                self.versions[source] += 1
