"""Validate retirement manifests and build version-fenced Graph operations."""

from dataclasses import dataclass
import hashlib
import json
import re
from uuid import UUID, uuid5


META = "aq:epic_feature_retirement"


class RetirementError(Exception):
    pass


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


@dataclass(frozen=True)
class KnowledgeRecord:
    node_id: str
    title: str
    data: dict

    def operations(self, evidence, *, content_type="decision"):
        return [
            create(
                self.node_id,
                "knowledge",
                self.title,
                content_type=content_type,
                metadata={META: self.data},
            ),
            verify(self.node_id, 1, evidence),
        ]


def trusted(node, owner, *, created=True):
    require(
        (not created or node["created_by"] == owner)
        and node["verification"] == "verified"
        and node["last_verified_by"] == owner,
        f"Record {node['id']} lacks current verification from the registered owner",
    )


def require_graph_owner(registration):
    spec = registration["metadata"].get(META, {}).get("spec", {})
    require(
        "owner_authority" not in spec,
        "This registration belongs to the AQ owner action journal. Use AQ owner operations.",
    )


def registration_spec(value, workspace, owner):
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
        workspace, f"aq/epic-retirement/1/{project}/{epic}/{generation}"
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
        "owner": owner,
        "ordinary_default": False,
        "permanent_behavior": nonempty(value["permanent_behavior"], 4096),
        "repositories": repository_flags(value["repositories"]),
    }
    return registration, spec


def decision_event(registration, value):
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
    return anchor, event


def incoming_blocks(edges, task):
    return {e for e in edges if e[0] == "blocks" and e[2] == task}
