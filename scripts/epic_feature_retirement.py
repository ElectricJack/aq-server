#!/usr/bin/env python3
"""Owner workflow for spec 0020 using the existing AQ Graph v1 API."""

import argparse
import json
import os
from pathlib import Path
from stat import S_ISREG

from epic_retirement_records import (
    decision_event,
    incoming_blocks,
    registration_spec,
    META,
    RetirementError,
    artifact,
    create as create,
    digest,
    fields,
    git_evidence,
    identity,
    link as link,
    nonempty,
    patch,
    proof_evidence,
    KnowledgeRecord,
    repository_flags,
    require,
    require_graph_owner,
    revision,
    revisions,
    sha as sha,
    task_evidence,
    trusted,
    uuid,
    verify,
)
from epic_retirement_setup import RetirementSetup
from epic_retirement_graph import (
    Graph,
    GraphError,
)
from epic_retirement_evidence import (
    missing_builds,
    missing_acceptance,
    apply_evidence,
    evidence_data,
)


class Retirement:
    def __init__(self, graph, owner):
        self.graph = graph
        self.owner = nonempty(owner, 128)

    def apply(self, anchor, title, operations):
        """Freeze the CAS batch as an owner-authenticated Graph proposal."""
        plan = {"name": title, "reason": title, "operations": operations}
        draft = self.graph.request(
            "/changesets",
            {
                **plan,
                "idempotency_key": f"epic/{anchor}/{digest(plan)}",
            },
        )
        require(
            draft["actor_id"] == self.owner,
            "Graph credential belongs to a different actor",
        )
        return self.graph.request(
            f"/changesets/{draft['id']}/apply",
            {"idempotency_key": f"epic/{anchor}/apply"},
        )

    def replay(self, anchor, expected):
        node = self.graph.node(anchor, missing=True, kind="knowledge")
        if node is None:
            return None
        trusted(node, self.owner)
        require(
            node["metadata"].get(META) == expected,
            f"{anchor} has different committed content",
        )
        return self.graph.applied(anchor, self.owner)

    def commit(self, anchor, payload, title, operations):
        try:
            return self.apply(anchor, title, operations)
        except GraphError as error:
            if error.status != 409:
                raise
            receipt = self.replay(anchor, payload)
            if receipt is None:
                raise
            return receipt

    def register_record(self, value):
        registration, spec = registration_spec(value, self.graph.workspace, self.owner)
        epic = spec["epic_id"]
        state = {
            "spec": spec,
            "repositories": spec["repositories"],
            "merges": {},
            "ordinary": {},
        }
        current = self.graph.node(registration, missing=True, kind="knowledge")
        if current is not None:
            self.check_existing_registration(
                current, spec, "Registration identity has a different specification"
            )
            return registration, self.graph.applied(registration, self.owner)
        parent = self.graph.node(epic, kind="task")
        require(
            parent["task_state"] in ("active", "draft"),
            "Register against an open epic task",
        )
        operations = KnowledgeRecord(
            registration, "Register epic feature flags", state
        ).operations(
            [task_evidence(epic, "epic")],
            content_type="policy",
        )
        try:
            receipt = self.apply(
                registration,
                "Register epic feature flags",
                [patch(parent), *operations],
            )
        except GraphError as error:
            if error.status != 409:
                raise
            current = self.graph.node(registration, missing=True, kind="knowledge")
            if current is None:
                raise
            self.check_existing_registration(
                current, spec, "A different registration won the race"
            )
            receipt = self.graph.applied(registration, self.owner)
        return registration, receipt

    def check_existing_registration(self, current, spec, mismatch):
        require_graph_owner(current)
        trusted(current, self.owner)
        require(current["metadata"][META]["spec"] == spec, mismatch)

    def load(self, registration):
        node = self.graph.node(uuid(registration), kind="knowledge")
        require_graph_owner(node)
        trusted(node, self.owner)
        state = node["metadata"].get(META)
        require(
            node["content_type"] == "policy"
            and isinstance(state, dict)
            and state.get("spec", {}).get("owner") == self.owner,
            "Expected this owner's epic retirement registration",
        )
        return node, state

    def save_state(self, node, state):
        return [
            patch(node, metadata={**node["metadata"], META: state}),
            verify(
                node["id"],
                node["version"] + 1,
                [task_evidence(state["spec"]["epic_id"], "epic")],
            ),
        ]

    def ensure(self, registration):
        node, state = self.load(registration)
        spec = state["spec"]
        setup = identity(registration, "setup")
        payload = {
            "registration": registration,
            "kind": "setup",
            **{key: spec[key] for key in ("approval", "cleanup", "closure_gate")},
        }
        receipt = self.replay(setup, payload)
        if receipt is not None:
            return receipt

        operations = RetirementSetup(self.graph, self.owner, registration, spec).plan()
        cleanup = spec["cleanup"]
        operations += KnowledgeRecord(
            setup, "Bind the epic retirement tasks", payload
        ).operations(
            [task_evidence(cleanup, "cleanup")],
            content_type="reference",
        )
        operations += self.save_state(node, {**state, "setup": setup})
        return self.commit(setup, payload, "Bind the epic retirement tasks", operations)

    def register(self, value):
        registration, receipt = self.register_record(value)
        return {
            "registration": registration,
            "registered": receipt,
            "setup": self.ensure(registration),
        }

    def amend(self, registration, value):
        fields(value, ("repositories",))
        additions = repository_flags(value["repositories"])
        node, state = self.load(registration)
        anchor = identity(registration, f"repositories/{digest(additions)}")
        event = {
            "registration": registration,
            "kind": "repositories",
            "additions": additions,
        }
        receipt = self.replay(anchor, event)
        if receipt is not None:
            return receipt
        require(
            not state.get("approved"),
            "An approved generation cannot change its repository set",
        )
        repos = state["repositories"]
        require(
            all(
                repo not in repos or repos[repo] == flag
                for repo, flag in additions.items()
            ),
            "Adding repositories cannot rename a registered flag",
        )
        updated = repository_flags({**repos, **additions})
        operations = KnowledgeRecord(
            anchor, "Record more affected repositories", event
        ).operations(
            [task_evidence(state["spec"]["epic_id"], "epic")],
            content_type="reference",
        )
        operations += self.save_state(
            node, {**state, "repositories": updated, "last_update": anchor}
        )
        return self.commit(
            anchor, event, "Add epic feature flag repositories", operations
        )

    def gates(self, state):
        spec = state["spec"]
        require(state.get("setup"), "Finish registration before deciding or closing")
        approval, cleanup, closure = (
            self.graph.node(spec[key], kind="task")
            for key in ("approval", "cleanup", "closure_gate")
        )
        for gate in (approval, closure):
            trusted(gate, self.owner)
        self.check_gate_dependencies(spec, (approval, cleanup, closure))
        return approval, cleanup, closure

    def check_gate_dependencies(self, spec, gates):
        approval, cleanup, closure = gates
        expected = {("blocks", approval["id"], cleanup["id"])}
        approval_edges = {
            e for e in self.graph.edges(approval["id"]) if e[0] == "blocks"
        }
        cleanup_edges = set(self.graph.edges(cleanup["id"]))
        require(
            approval_edges == expected
            and incoming_blocks(cleanup_edges, cleanup["id"]) == expected
            and ("parent-child", spec["epic_id"], cleanup["id"]) in cleanup_edges
            and ("parent-child", spec["epic_id"], closure["id"])
            in self.graph.edges(closure["id"]),
            "The retirement dependencies no longer match the registration",
        )

    def decide(self, registration, value):
        fields(value, ("revision", "choice", "vetted", "evidence"))
        node, state = self.load(registration)
        anchor, event = decision_event(registration, value)
        number, choice = event["revision"], event["choice"]
        vetted, evidence = event["vetted"], event["evidence"]
        receipt = self.replay(anchor, event)
        if receipt is not None:
            return receipt
        revisions(vetted, state["repositories"])
        require(
            not state.get("approved"),
            "This generation has already released its cleanup",
        )
        require(
            number == state.get("decision_revision", 0) + 1,
            "Decision revision must follow the last decision",
        )
        approval, cleanup, _ = self.gates(state)
        trusted(cleanup, self.owner)
        require(
            approval["task_state"] == "draft" and cleanup["task_state"] == "draft",
            "Approval and cleanup must both still be draft",
        )
        operations = KnowledgeRecord(
            anchor, "Record owner retirement decision", event
        ).operations(
            [*git_evidence(vetted), proof_evidence(evidence, "integrated-vetting")],
        )
        updated = {**state, "decision": anchor, "decision_revision": number}
        if choice == "approve":
            operations += [
                patch(
                    approval,
                    task_state="completed",
                    metadata={
                        **approval["metadata"],
                        META: {"registration": registration, "decision": anchor},
                    },
                ),
                verify(
                    approval["id"],
                    approval["version"] + 1,
                    [proof_evidence(evidence, "integrated-vetting")],
                ),
                patch(
                    cleanup,
                    task_state="active",
                    metadata={
                        **cleanup["metadata"],
                        META: {"registration": registration, "approval": anchor},
                    },
                ),
            ]
            updated["approved"] = anchor
        operations += self.save_state(node, updated)
        return self.commit(
            anchor, event, "Record owner retirement decision", operations
        )

    def approved(self, state):
        anchor = state.get("approved")
        require(anchor, "Approval is still pending")
        node = self.graph.node(anchor, kind="knowledge")
        trusted(node, self.owner)
        decision = node["metadata"][META]
        require(
            decision["kind"] == "decision"
            and decision["choice"] == "approve"
            and decision["revision"] == state["decision_revision"]
            and decision["vetted"].keys() == state["repositories"].keys(),
            "Approved revision no longer matches the registration",
        )
        return node, decision

    def evidence(self, registration, value):
        kind, data = evidence_data(value)
        proof, number = (
            artifact(value["evidence"]),
            revision(value["approval_revision"]),
        )
        node, state = self.load(registration)
        event = {
            "registration": registration,
            "kind": kind,
            "approval_revision": number,
            "evidence": proof,
            **data,
        }
        anchor = identity(registration, f"evidence/{digest(event)}")
        receipt = self.replay(anchor, event)
        if receipt is not None:
            return receipt

        require(not state.get("closed"), "This epic retirement is closed")
        _, decision = self.approved(state)
        require(
            number == decision["revision"],
            "Evidence must name the approved decision revision",
        )
        approval, _, _ = self.gates(state)
        require(
            approval["task_state"] == "completed",
            "Approval gate has not released cleanup",
        )
        current = self.evidence_nodes(state)
        state, commits = apply_evidence(state, current, event, anchor)
        operations = KnowledgeRecord(
            anchor, "Record epic retirement evidence", event
        ).operations(
            [*git_evidence(commits), proof_evidence(proof, kind)],
            content_type="reference",
        )
        operations += self.save_state(node, state)
        return self.commit(anchor, event, "Record epic retirement evidence", operations)

    def evidence_nodes(self, state):
        nodes = []

        def read(anchor, kind):
            node = self.graph.node(anchor, kind="knowledge")
            trusted(node, self.owner)
            data = node["metadata"][META]
            require(
                data["kind"] == kind
                and data["approval_revision"] == state["decision_revision"],
                "Retirement evidence has the wrong kind or approval revision",
            )
            nodes.append(node)
            return data

        return {
            "merges": {
                repo: read(anchor, "merge") for repo, anchor in state["merges"].items()
            },
            "ordinary": {
                repo: read(anchor, "ordinary")
                for repo, anchor in state["ordinary"].items()
            },
            "integrated": read(state["integrated"], "integrated")
            if state.get("integrated")
            else None,
            "nodes": nodes,
        }

    def outstanding(self, state, evidence, cleanup):
        yield from missing_builds(state, evidence)
        yield from missing_acceptance(evidence)
        if cleanup["task_state"] != "completed":
            yield f"Cleanup task {cleanup['id']} has not completed through normal delivery"
        spec = state["spec"]
        tree = self.graph.request(f"/nodes/{spec['epic_id']}/tree?depth=1")
        require(not tree["truncated"], "Epic task list is truncated")
        for child in tree["children"]:
            task = child["node"]
            if task["id"] not in (spec["cleanup"], spec["closure_gate"]) and task[
                "task_state"
            ] not in ("completed", "canceled"):
                yield f"Epic task {task['id']} is still open"

    def status(self, registration):
        _, state = self.load(registration)
        spec = state["spec"]
        result = {
            key: spec[key]
            for key in ("epic_id", "project_id", "approval", "cleanup", "closure_gate")
        }
        result |= {
            "registration": registration,
            "repositories": state["repositories"],
            "decision_revision": state.get("decision_revision", 0),
        }
        if state.get("closed"):
            return {**result, "status": "Retired", "closure": state["closed"]}
        if not state.get("setup"):
            return {**result, "status": "Registration pending"}
        if not state.get("approved"):
            return {
                **result,
                "status": "Changes requested"
                if state.get("decision")
                else "Approval pending",
            }
        _, cleanup, _ = self.gates(state)
        return {
            **result,
            "status": "Approved · flag removal pending",
            "merge_receipts": state["merges"],
            "outstanding": list(
                self.outstanding(state, self.evidence_nodes(state), cleanup)
            ),
        }

    def close(self, registration):
        node, state = self.load(registration)
        anchor = identity(registration, "close")
        if state.get("closed"):
            require(state["closed"] == anchor, "Wrong retirement closure identity")
            retained = self.graph.node(anchor, kind="knowledge")
            trusted(retained, self.owner)
            return self.graph.applied(anchor, self.owner)
        approval, cleanup, closure = self.gates(state)
        decision, approved = self.approved(state)
        require(
            approval["task_state"] == "completed" and closure["task_state"] == "draft",
            "Retirement gates are in the wrong state for closure",
        )
        evidence = self.evidence_nodes(state)
        failures = list(self.outstanding(state, evidence, cleanup))
        require(not failures, ", ".join(failures))
        epic = self.graph.node(state["spec"]["epic_id"], kind="task")
        require(
            epic["task_state"] in ("draft", "active"),
            "The epic was closed outside the workflow",
        )
        event = {
            "registration": registration,
            "kind": "closure",
            "approval_revision": approved["revision"],
            "decision": decision["id"],
            "cleanup": cleanup["id"],
            "merges": state["merges"],
            "ordinary": state["ordinary"],
            "integrated": state["integrated"],
        }
        proof = proof_evidence(
            evidence["integrated"]["evidence"], "integrated-acceptance"
        )
        operations = [
            patch(item) for item in (approval, cleanup, decision, *evidence["nodes"])
        ]
        operations += KnowledgeRecord(
            anchor, "Close epic feature retirement", event
        ).operations(
            [proof, task_evidence(cleanup["id"], "cleanup")],
        )
        operations += [
            patch(
                closure,
                task_state="completed",
                metadata={
                    **closure["metadata"],
                    META: {"registration": registration, "receipt": anchor},
                },
            ),
            verify(closure["id"], closure["version"] + 1, [proof]),
            patch(epic, task_state="completed"),
        ]
        operations += self.save_state(node, {**state, "closed": anchor})
        return self.commit(anchor, event, "Close epic feature retirement", operations)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--graph-origin", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument(
        "--owner", required=True, help="Authenticated Graph owner actor ID"
    )
    parser.add_argument(
        "--token-file", required=True, type=Path, help="Private Graph credential file"
    )
    actions = parser.add_subparsers(dest="action", required=True)
    actions.add_parser("register").add_argument("manifest", type=Path)
    for action in ("ensure", "amend", "decide", "evidence", "status", "close"):
        command = actions.add_parser(action)
        command.add_argument("registration")
        if action in ("amend", "decide", "evidence"):
            command.add_argument("manifest", type=Path)
    args = parser.parse_args()
    try:
        with args.token_file.open() as token_file:
            mode = os.fstat(token_file.fileno()).st_mode
            require(
                S_ISREG(mode) and mode & 0o077 == 0,
                "Graph token file must be a private regular file",
            )
            token = token_file.read(4097).strip()
        require(32 <= len(token.encode()) <= 4096, "Invalid Graph token length")
        workflow = Retirement(
            Graph(args.graph_origin, args.workspace, token), args.owner
        )
        inputs = [] if args.action == "register" else [uuid(args.registration)]
        if hasattr(args, "manifest"):
            inputs.append(json.loads(args.manifest.read_text()))
        response = getattr(workflow, args.action)(*inputs)
        print(json.dumps(response, indent=2, sort_keys=True, ensure_ascii=False))
    except (RetirementError, OSError, ValueError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
