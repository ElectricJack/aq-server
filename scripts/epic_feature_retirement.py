#!/usr/bin/env python3
"""Owner workflow for spec 0020 using the existing AQ Graph v1 API."""

import argparse
import hashlib
import json
import os
import re
import time
from pathlib import Path
from stat import S_ISREG
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener
from uuid import UUID, uuid5


META = "aq:epic_feature_retirement"


class RetirementError(Exception):
    pass


class GraphError(RetirementError):
    def __init__(self, status, payload):
        self.status = status
        super().__init__(f"Graph HTTP {status}: {payload}")


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def digest(value):
    return hashlib.sha256(canonical(value)).hexdigest()


def identity(namespace, key):
    return str(uuid5(UUID(namespace), key))


def require(yes, message):
    if not yes:
        raise RetirementError(message)


def fields(value, required, optional=()):
    require(
        isinstance(value, dict)
        and set(required) <= value.keys() <= set(required) | set(optional),
        f"Expected fields {', '.join(required)}"
        + (f" with optional {', '.join(optional)}" if optional else ""),
    )


def uuid(value):
    try:
        parsed = UUID(value)
        require(parsed.int != 0, "A nil UUID is not a valid identity")
        return str(parsed)
    except (TypeError, ValueError, AttributeError):
        raise RetirementError("Expected a UUID") from None


def nonempty(value, limit):
    require(
        isinstance(value, str) and value.strip() and len(value.encode()) <= limit,
        f"Expected nonempty text of at most {limit} bytes",
    )
    return value


def revision(value):
    require(
        type(value) is int and 0 < value < 2**63, "Expected a positive integer revision"
    )
    return value


def sha(value):
    require(
        isinstance(value, str)
        and re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", value)
        and any(c != "0" for c in value),
        "Expected a full nonzero commit SHA",
    )
    return value


def artifact(value):
    fields(value, ("artifact_id", "sha256"))
    require(
        isinstance(value["sha256"], str)
        and re.fullmatch(r"[0-9a-f]{64}", value["sha256"]),
        "Expected a 64-digit SHA-256",
    )
    return {"artifact_id": uuid(value["artifact_id"]), "sha256": value["sha256"]}


def revisions(value, repositories):
    require(
        isinstance(value, dict) and value.keys() == repositories.keys(),
        "Evidence must name the exact registered repositories",
    )
    return {repo: sha(commit) for repo, commit in value.items()}


def repository_flags(value):
    require(
        isinstance(value, dict) and 1 <= len(value) <= 32,
        "Register between one and 32 repository flag names",
    )
    for repo, flag in value.items():
        nonempty(repo, 96)
        require(
            isinstance(flag, str)
            and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_.-]{0,127}", flag),
            f"Invalid flag name for {repo}",
        )
    return value


def graph_origin(value):
    parts = urlsplit(value)
    loopback = parts.hostname in ("127.0.0.1", "::1", "localhost")
    require(
        parts.hostname
        and not any((parts.username, parts.password, parts.query, parts.fragment))
        and (parts.scheme == "https" or (parts.scheme == "http" and loopback))
        and parts.path in ("", "/"),
        "Use an HTTPS or loopback Graph origin",
    )
    return value.rstrip("/")


class Graph:
    def __init__(self, origin, workspace, token):
        self.workspace = uuid(workspace)
        self.base = graph_origin(origin) + f"/v1/workspaces/{self.workspace}"
        self.token = token
        self.http = build_opener(ProxyHandler({}), NoRedirect())

    def request(self, path, body=None, *, missing=False):
        request = Request(
            self.base + path,
            data=None if body is None else canonical(body),
            headers={
                "Authorization": f"Bearer {self.token}",
                "Content-Type": "application/json",
                "X-AQ-Graph-API-Version": "1",
            },
        )
        for attempt in range(3):
            try:
                with self.http.open(request, timeout=10) as reply:
                    return json.load(reply)
            except HTTPError as reply:
                if reply.code == 404 and missing:
                    return None
                if reply.code != 503 or attempt == 2:
                    raise GraphError(reply.code, json.load(reply)) from None
            except (URLError, TimeoutError) as error:
                if attempt == 2:
                    raise RetirementError(
                        "Graph response unavailable. Retry the same command to recover."
                    ) from error
            time.sleep(0.25 * (attempt + 1))

    def node(self, node_id, *, missing=False, kind=None):
        node = self.request(f"/nodes/{node_id}", missing=missing)
        if node is not None:
            require(
                not node["deleted"]
                and not node["retired"]
                and (kind is None or node["kind"] == kind),
                f"Unavailable {kind or 'record'} {node_id}",
            )
        return node

    def edges(self, node_id):
        context = self.request(f"/nodes/{node_id}/context")
        require(not context["truncated"], f"Dependency context truncated for {node_id}")
        return [(e["edge_type"], e["source"], e["target"]) for e in context["edges"]]

    def applied(self, anchor, owner):
        history = self.request(f"/nodes/{anchor}/history?limit=1")["items"]
        require(
            history and history[0]["operation"] == "created",
            f"No creation receipt for {anchor}",
        )
        change = self.request(f"/changesets/{history[0]['change_set_id']}")
        require(
            change["status"] == "applied"
            and change["actor_id"] == owner
            and change["result"],
            f"No owner-applied receipt for {anchor}",
        )
        return change["result"]


def create(node_id, kind, title, **data):
    return {"op": "create_node", "id": node_id, "kind": kind, "title": title, **data}


def patch(node, **data):
    return {
        "op": "update_node",
        "id": node["id"],
        "expected_version": node["version"],
        "patch": data,
    }


def link(registration, kind, source, target):
    return {
        "op": "add_edge",
        "id": identity(registration, f"{kind}/{source}/{target}"),
        "edge_type": kind,
        "source": source,
        "target": target,
    }


def verify(node_id, version, evidence):
    return {
        "op": "verify_node",
        "id": node_id,
        "expected_version": version,
        "verification": "verified",
        "evidence": evidence,
    }


def task_evidence(task, label):
    return {"kind": "task", "source_id": label, "task_id": task}


def proof_evidence(proof, label):
    return {"kind": "artifact", "source_id": label, **proof}


def git_evidence(repos):
    return [
        {
            "kind": "git",
            "source_id": f"revision/{repo}",
            "repository": repo,
            "commit": commit,
            "path": ".",
        }
        for repo, commit in sorted(repos.items())
    ]


def record(node_id, title, data, evidence, *, content_type="decision"):
    return [
        create(
            node_id,
            "knowledge",
            title,
            content_type=content_type,
            metadata={META: data},
        ),
        verify(node_id, 1, evidence),
    ]


def trusted(node, owner, *, created=True):
    require(
        (not created or node["created_by"] == owner)
        and node["verification"] == "verified"
        and node["last_verified_by"] == owner,
        f"Record {node['id']} lacks current verification from the registered owner",
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
        fields(
            value,
            (
                "epic_id",
                "project_id",
                "generation",
                "repositories",
                "ordinary_default",
                "permanent_behavior",
            ),
            ("adopt",),
        )
        epic, project = uuid(value["epic_id"]), uuid(value["project_id"])
        generation = revision(value["generation"])
        require(value["ordinary_default"] is False, "Incomplete flags must default off")
        registration = identity(
            self.graph.workspace, f"aq/epic-retirement/1/{project}/{epic}/{generation}"
        )
        adopt = value.get("adopt")
        if adopt is not None:
            fields(adopt, ("approval", "cleanup"))
            adopt = {key: uuid(item) for key, item in adopt.items()}
        ids = adopt or {
            name: identity(registration, name) for name in ("approval", "cleanup")
        }
        ids = {**ids, "closure_gate": identity(registration, "closure-gate")}
        require(
            len({epic, registration, *ids.values()}) == 5,
            "Retirement nodes must have distinct IDs",
        )
        spec = {
            "epic_id": epic,
            "project_id": project,
            "generation": generation,
            **ids,
            "adopt": adopt is not None,
            "owner": self.owner,
            "ordinary_default": False,
            "permanent_behavior": nonempty(value["permanent_behavior"], 4096),
            "repositories": repository_flags(value["repositories"]),
        }
        state = {
            "spec": spec,
            "repositories": spec["repositories"],
            "merges": {},
            "ordinary": {},
        }
        current = self.graph.node(registration, missing=True, kind="knowledge")
        if current is not None:
            trusted(current, self.owner)
            require(
                current["metadata"][META]["spec"] == spec,
                "Registration identity has a different specification",
            )
            return registration, self.graph.applied(registration, self.owner)
        parent = self.graph.node(epic, kind="task")
        require(
            parent["task_state"] in ("active", "draft"),
            "Register against an open epic task",
        )
        operations = record(
            registration,
            "Register epic feature flags",
            state,
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
            trusted(current, self.owner)
            require(
                current["metadata"][META]["spec"] == spec,
                "A different registration won the race",
            )
            receipt = self.graph.applied(registration, self.owner)
        return registration, receipt

    def load(self, registration):
        node = self.graph.node(uuid(registration), kind="knowledge")
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

        epic = self.graph.node(spec["epic_id"], kind="task")
        require(epic["task_state"] in ("draft", "active"), "Epic is closed")
        approval, cleanup, closure = (
            spec[key] for key in ("approval", "cleanup", "closure_gate")
        )
        operations = [patch(epic)]
        versions = {}
        for key, title in [
            ("approval", "Approve integrated epic feature"),
            ("cleanup", "Remove the epic feature flags"),
            ("closure_gate", "Verify epic retirement evidence"),
        ]:
            task_id = spec[key]
            existing = self.graph.node(task_id, missing=True, kind="task")
            marker = {"registration": registration, "role": key}
            if spec["adopt"] and key != "closure_gate":
                require(
                    existing
                    and existing["task_state"] == "draft"
                    and existing["created_by"] == self.owner,
                    f"Adopt only an owner-created draft {key}",
                )
                metadata = existing["metadata"]
                require(
                    META not in metadata or metadata[META] == marker,
                    f"{task_id} is marked for a different retirement",
                )
                operations.append(patch(existing, metadata={**metadata, META: marker}))
                versions[task_id] = existing["version"] + int(META not in metadata)
            else:
                require(
                    existing is None,
                    f"Retirement task {task_id} exists without a setup receipt",
                )
                body = {
                    "approval": "Owner gate. Record the vetted revisions and integrated test evidence through the retirement workflow.",
                    "cleanup": f"{spec['permanent_behavior']} Record cleanup merges, ordinary builds and the integrated acceptance check.",
                    "closure_gate": "Owner gate. The retirement workflow closes this task after validating all cleanup evidence.",
                }[key]
                operations.append(
                    create(
                        task_id,
                        "task",
                        title,
                        task_state="draft",
                        body=body,
                        metadata={META: marker},
                    )
                )
                versions[task_id] = 1

        def ensure_edge(kind, source, target):
            current = (
                self.graph.edges(target)
                if spec["adopt"] and target in (approval, cleanup)
                else []
            )
            same_kind = [
                edge for edge in current if edge[0] == kind and edge[2] == target
            ]
            require(
                not same_kind or same_kind == [(kind, source, target)],
                f"Unexpected {kind} dependencies for {target}",
            )
            if not same_kind:
                operations.append(link(registration, kind, source, target))
                versions[target] += 1
                if source in versions:
                    versions[source] += 1

        if spec["adopt"]:
            attached = self.graph.edges(approval)
            require(
                all(
                    kind != "blocks" or (source == approval and target == cleanup)
                    for kind, source, target in attached
                ),
                "Adopted approval has dependencies beyond this cleanup",
            )
        ensure_edge("blocks", approval, cleanup)
        ensure_edge("parent-child", epic["id"], cleanup)
        ensure_edge("parent-child", epic["id"], closure)
        operations += [
            verify(task, versions[task], [task_evidence(epic["id"], "epic")])
            for task in (approval, cleanup, closure)
        ]
        operations += record(
            setup,
            "Bind the epic retirement tasks",
            payload,
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
        operations = record(
            anchor,
            "Record more affected repositories",
            event,
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
        expected = {("blocks", approval["id"], cleanup["id"])}
        approval_edges = {
            e for e in self.graph.edges(approval["id"]) if e[0] == "blocks"
        }
        cleanup_edges = set(self.graph.edges(cleanup["id"]))
        require(
            approval_edges == expected
            and {e for e in cleanup_edges if e[0] == "blocks" and e[2] == cleanup["id"]}
            == expected
            and ("parent-child", spec["epic_id"], cleanup["id"]) in cleanup_edges
            and ("parent-child", spec["epic_id"], closure["id"])
            in self.graph.edges(closure["id"]),
            "The retirement dependencies no longer match the registration",
        )
        return approval, cleanup, closure

    def decide(self, registration, value):
        fields(value, ("revision", "choice", "vetted", "evidence"))
        node, state = self.load(registration)
        number, choice = revision(value["revision"]), value["choice"]
        require(
            choice in ("approve", "request_changes"),
            "Expected approve or request_changes",
        )
        # Replays may refer to the pre-amendment repository set.
        require(isinstance(value["vetted"], dict), "Expected vetted revisions")
        vetted = {repo: sha(commit) for repo, commit in value["vetted"].items()}
        evidence = artifact(value["evidence"])
        anchor = identity(registration, f"decision/{number}")
        event = {
            "registration": registration,
            "kind": "decision",
            "revision": number,
            "choice": choice,
            "vetted": vetted,
            "evidence": evidence,
        }
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
        operations = record(
            anchor,
            "Record owner retirement decision",
            event,
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
        require(isinstance(value, dict), "Expected an evidence object")
        kind = value.get("kind")
        common = ("kind", "approval_revision", "evidence")
        if kind == "merge":
            fields(value, (*common, "repository", "pull_request", "sha"))
            url = urlsplit(nonempty(value["pull_request"], 2048))
            require(
                url.scheme == "https"
                and url.hostname
                and not (url.username or url.password),
                "Supply an HTTPS pull request URL",
            )
            data = {
                "repository": nonempty(value["repository"], 96),
                "pull_request": value["pull_request"],
                "sha": sha(value["sha"]),
            }
        elif kind == "ordinary":
            fields(value, (*common, "repository", "sha", "passed", "flag_absent"))
            require(type(value["flag_absent"]) is bool, "flag_absent must be boolean")
            data = {
                "repository": nonempty(value["repository"], 96),
                "sha": sha(value["sha"]),
                "flag_absent": value["flag_absent"],
            }
        elif kind == "integrated":
            fields(value, (*common, "revisions", "builds", "passed"))
            require(
                isinstance(value["revisions"], dict)
                and isinstance(value["builds"], dict),
                "Supply a revision and ordinary build artifact for each repository",
            )
            data = {
                "revisions": {
                    repo: sha(commit) for repo, commit in value["revisions"].items()
                },
                "builds": {
                    repo: artifact(build) for repo, build in value["builds"].items()
                },
            }
        else:
            raise RetirementError("Expected merge, ordinary or integrated evidence")
        if kind != "merge":
            require(type(value["passed"]) is bool, "passed must be boolean")
            data["passed"] = value["passed"]

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
        merges, ordinary = current["merges"], current["ordinary"]
        if kind in ("merge", "ordinary"):
            repo = data["repository"]
            require(repo in state["repositories"], f"{repo} is not registered")
        if kind == "merge":
            require(repo not in merges, f"A merge was already recorded for {repo}")
            state = {**state, "merges": {**state["merges"], repo: anchor}}
            commits = {repo: data["sha"]}
        elif kind == "ordinary":
            require(
                repo in merges and merges[repo]["sha"] == data["sha"],
                f"Ordinary build for {repo} must match its recorded merge SHA",
            )
            state = {**state, "ordinary": {**state["ordinary"], repo: anchor}}
            commits = {repo: data["sha"]}
        else:
            commits = {repo: item["sha"] for repo, item in merges.items()}
            require(
                data["revisions"] == commits
                and commits.keys() == state["repositories"].keys(),
                "Integrated check must name all recorded cleanup merge SHAs",
            )
            builds = {
                repo: item["evidence"]
                for repo, item in ordinary.items()
                if item["passed"] and item["flag_absent"]
            }
            require(
                data["builds"] == builds and builds.keys() == commits.keys(),
                "Integrated check must name the passing flag-free ordinary builds",
            )
            state = {**state, "integrated": anchor}
        operations = record(
            anchor,
            "Record epic retirement evidence",
            event,
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
        merges, ordinary, integrated = (
            evidence[key] for key in ("merges", "ordinary", "integrated")
        )
        for repo in sorted(state["repositories"]):
            if repo not in merges:
                yield f"{repo}: cleanup merge pending"
                continue
            build = ordinary.get(repo)
            if not build or build["sha"] != merges[repo]["sha"]:
                yield f"{repo}: ordinary build evidence pending"
            elif not build["passed"] or not build["flag_absent"]:
                yield f"{repo}: ordinary build or flag removal check failed"
        if not integrated:
            yield "Integrated acceptance pending"
        elif not integrated["passed"]:
            yield "Integrated acceptance failed"
        elif integrated["revisions"] != {
            repo: item["sha"] for repo, item in merges.items()
        } or integrated["builds"] != {
            repo: item["evidence"] for repo, item in ordinary.items()
        }:
            yield "Integrated acceptance evidence no longer matches the ordinary builds"
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
        operations += record(
            anchor,
            "Close epic feature retirement",
            event,
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
