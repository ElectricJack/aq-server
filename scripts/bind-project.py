#!/usr/bin/env python3
"""Make an already-registered project able to take work on a stack (spec 0014).

  scripts/bind-project.py PROJECT_ID                       # show what would change
  scripts/bind-project.py PROJECT_ID --apply               # change it
  scripts/bind-project.py PROJECT_ID --pin COMMIT --apply  # start tasks from one commit
  scripts/bind-project.py PROJECT_ID --branch refs/heads/main --apply   # follow a branch

A registered project is one the server already holds a binding record for
(`GET /api/projects/PROJECT_ID`): repository onboarding or the development seed
created it. This script then does what is left, for any project:

1. Checks that the default role and the default intelligence profile exist as
   Settings documents, and reads the role's execution profile.
2. Saves the binding: enabled, `default_role`, `default_intelligence`, and
   ceilings that hold the permissions the role asks for (its operations, scoped
   to this project and its repository). Existing ceilings, nodes, groups,
   environments, role notes and publication are kept; the execution profile's
   environment is added to the allowed environments. A ceiling below the
   execution profile's limit is reported and raised only with --raise-ceilings.
3. With --pin or --branch, or when the server has no `[profiled_execution]`
   entry for the project: syncs the branch in the stack's central aq-git and
   retains the starting commit (a retention handle holds one commit).
4. Prints the exact `[profiled_execution] projects` entry when the config has
   none or the source changes. That list is server configuration (source,
   retention handle, priority; part of the configuration digest), so this script
   never edits a vault: add the entry, raise `[profiled_execution] revision`
   and restart the server. `scripts/ms1-repin-project.py` does that edit for an
   aq-ms1 stack whose entry already exists.

Nothing per project is added to roles or profiles: none of them names a project.

It talks to the server's owner HTTP API (--server, else AQ_DEV_SERVER_URL) and
to aq-git through its request interface in the stack's aq-git container
(--stack, else COMPOSE_PROJECT_NAME, else aq-ms1). Without --apply it only
reads: no binding is saved and aq-git is asked only to show the repository.
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid

DEFAULT_ROLE = "developer"
DEFAULT_INTELLIGENCE = "standard-high"
DEFAULT_BRANCH = "refs/heads/main"
# Execution profile limit -> the project ceiling that bounds it.
LIMIT_CEILINGS = (("startup_context_bytes", "context_bytes"), ("wall_time_ms", "wall_time_ms"),
                  ("memory_bytes", "memory_bytes"), ("cpu_millis", "cpu_millis"),
                  ("process_limit", "process_limit"))


class Refused(Exception):
    pass


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def http(url, body=None):
    request = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json", "X-AQ-Request": "1"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            text = response.read()
            return json.loads(text) if text else None
    except urllib.error.HTTPError as error:
        if error.code == 404 and body is None:
            return None
        raise Refused(f"{url} -> {error.code} {error.read()[:600]!r}") from None
    except urllib.error.URLError as error:
        raise Refused(f"{url} is not reachable: {error.reason}") from None


def git_request(stack, repository, generation, operation, params=None, mutation=None):
    """One request to aq-git's public request interface, as the other stack scripts make it."""
    request = dict(protocol_version=1, request_id=str(uuid.uuid4()), operation=operation, params=params or {},
                   deadline_ms=120000, repository_id=repository, expected_binding_generation=generation)
    if mutation:
        request["operation_id"] = mutation
    raw = subprocess.run(["docker", "exec", "-i", f"{stack}-aq-git-1", "aq-git", "--socket",
                          "/run/aq-git/central.sock", "--input", "-", "--json", "request"],
                         input=json.dumps(request) + "\n", capture_output=True, text=True, timeout=600).stdout
    try:
        response = json.loads(raw)
    except ValueError:
        raise Refused(f"aq-git gave no answer to {operation} (is {stack}-aq-git-1 running?)") from None
    if response.get("ok") is not True:
        raise Refused(f"aq-git refused {operation}: {json.dumps(response.get('error'))[:500]}")
    return response["result"]


def requested_permissions(operations, project, repository):
    """Each operation of a role with the scope the server gives it (spec 0014 §6.2):
    the project, for `source.*` the project's repository, for `wait.*` the attempt itself.
    An attempt holds one only when the project's ceilings list exactly that entry.
    """
    out = []
    for operation in operations:
        if operation.startswith("source."):
            scope = f"repository:{repository}"
        elif operation.startswith("wait."):
            scope = "attempt:self"
        else:
            scope = f"project:{project}"
        entry = dict(operation=operation, scope_reference=scope)
        if entry not in out:
            out.append(entry)
    return out


def plan_binding(current, role, level, role_fields, execution_fields, raise_ceilings=False):
    """The binding to save and notes on what differs. Pure."""
    binding = dict(current)
    notes = []
    project, repository = binding["project_id"], binding["repository_id"]
    binding["enabled"] = True
    # The server leaves a default out of the binding it returns, so only a
    # non-default is written: otherwise every run would look like a change.
    for key, value, default in (("default_role", role, DEFAULT_ROLE),
                                ("default_intelligence", level, DEFAULT_INTELLIGENCE)):
        if value == default:
            binding.pop(key, None)
        else:
            binding[key] = value
    ceilings = dict(binding["ceilings"])
    permissions = list(ceilings.get("permissions", []))
    for entry in requested_permissions(role_fields.get("operations", []), project, repository):
        if entry not in permissions:
            permissions.append(entry)
            notes.append(f"ceilings: permit {entry['operation']} on {entry['scope_reference']}")
    ceilings["permissions"] = permissions
    limits = execution_fields.get("limits", {})
    for limit, ceiling in LIMIT_CEILINGS:
        wanted, held = limits.get(limit), ceilings.get(ceiling)
        if isinstance(wanted, int) and isinstance(held, int) and held < wanted:
            if raise_ceilings:
                ceilings[ceiling] = wanted
                notes.append(f"ceilings: {ceiling} raised from {held} to {wanted}")
            else:
                notes.append(f"WARNING: ceiling {ceiling}={held} is below the execution profile's "
                             f"{limit}={wanted}; pass --raise-ceilings to raise it")
    binding["ceilings"] = ceilings
    environment = execution_fields.get("environment")
    environments = list(binding.get("allowed_environments", []))
    if environment and environment not in environments:
        environments.append(environment)
        notes.append(f"allowed_environments: add {environment}")
    binding["allowed_environments"] = environments
    return binding, notes


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, dict):
        return "{" + ", ".join(f"{key} = {toml_value(item)}" for key, item in value.items()) + "}"
    raise ValueError(type(value).__name__)


def project_entry(project, source, retention_handle, priority=0):
    return dict(project_id=project, enabled=True, priority=priority, source=source,
                retention_handle=retention_handle)


def render_entry(entry):
    """The entry in both spellings TOML allows; a vault uses one of them."""
    table = "[[profiled_execution.projects]]\n" + "".join(
        f"{key} = {toml_value(value)}\n" for key, value in entry.items())
    return (f"# as a table, when the vault lists projects as [[profiled_execution.projects]]:\n{table}"
            f"# or as one element of an inline `projects = [...]` list:\n{toml_value(entry)}\n")


def source_selection(pin, branch, head):
    if branch:
        return dict(kind="named_ref", reference=branch)
    return dict(kind="immutable", object=dict(format="sha1", hex=pin or head))


def retain(stack, repository, generation, branch, commit, tag):
    """Sync `branch`, then hold `commit` (default: the branch head) under a new retention handle."""
    def op(step):
        return str(uuid.uuid5(uuid.NAMESPACE_URL, f"bind-project-{repository}-{tag}-{step}"))
    synced = git_request(stack, repository, generation, "repo.sync", dict(refs=[branch]), mutation=op("sync"))
    head = synced.get("refs", {}).get(branch)
    if not (isinstance(head, str) and len(head) == 40):
        raise Refused(f"sync gave no commit for {branch}")
    base = dict(algorithm="sha1", hex=commit or head)
    created = git_request(stack, repository, generation, "workspace.create", dict(base=base),
                          mutation=op("workspace"))
    checkpoint = git_request(stack, repository, generation, "workspace.checkpoint", dict(
        workspace_id=created["workspace_id"], expected_generation=1, expected_head=base, quiescent=True,
        mode="head"), mutation=op("checkpoint"))
    git_request(stack, repository, generation, "workspace.retire", dict(
        workspace_id=created["workspace_id"], expected_generation=checkpoint["workspace_generation"],
        expected_head=base, quiescent=True), mutation=op("retire"))
    retained = git_request(stack, repository, generation, "retention.inspect",
                           dict(handle_id=checkpoint["retention_handle"]))
    if retained["state"] != "active" or not any(r["oid"] == base["hex"] for r in retained["refs"]):
        raise Refused("the retention handle does not hold the starting commit")
    return base["hex"], checkpoint["retention_handle"]


def configured(server, project):
    """Whether `[profiled_execution] projects` lists the project: None, or its `enabled`."""
    after = None
    for _ in range(64):
        query = "?limit=100" + (f"&after={after}" if after else "")
        page = http(server + "/api/projects" + query)
        if project in page.get("execution", {}):
            return page["execution"][project]
        after = page.get("next_cursor")
        if not after:
            return None
    return None


def document(server, kind, identifier):
    found = http(f"{server}/api/settings/{kind}/{urllib.parse.quote(identifier, safe='')}")
    if found is None:
        raise Refused(f"no {kind} document `{identifier}` exists; see GET {server}/api/settings/{kind}")
    if found.get("problem"):
        raise Refused(f"{kind} document `{identifier}` does not parse: {found['problem']}")
    return found["fields"]


def arguments(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("project", type=lambda text: str(uuid.UUID(text)))
    parser.add_argument("--default-role", default=DEFAULT_ROLE)
    parser.add_argument("--default-intelligence", default=DEFAULT_INTELLIGENCE)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--pin", metavar="COMMIT", help="start every task from this commit (40 hex digits)")
    source.add_argument("--branch", metavar="REF", help="start every task from this branch's head, e.g. refs/heads/main")
    parser.add_argument("--raise-ceilings", action="store_true",
                        help="raise a ceiling that is below the execution profile's limit")
    parser.add_argument("--apply", action="store_true", help="save the binding and retain the source")
    parser.add_argument("--server", default=os.environ.get("AQ_DEV_SERVER_URL", "http://127.0.0.1:28080"))
    parser.add_argument("--stack", default=os.environ.get("COMPOSE_PROJECT_NAME", "aq-ms1"))
    args = parser.parse_args(argv)
    if args.pin and not (len(args.pin) == 40 and all(c in "0123456789abcdef" for c in args.pin)):
        parser.error("--pin takes a full lowercase 40-digit commit id")
    if args.branch and not args.branch.startswith("refs/heads/"):
        parser.error("--branch takes a full ref such as refs/heads/main")
    args.server = args.server.rstrip("/")
    return args


def run(args, out=print):
    server, project = args.server, args.project
    record = http(f"{server}/api/projects/{project}")
    if record is None:
        raise Refused("the server has no binding record for this project. Register it first "
                      "(POST /api/repositories/onboard, or the development seed), then run this again")
    current = record["binding"]
    role = document(server, "roles", args.default_role)
    document(server, "intelligence-profiles", args.default_intelligence)
    execution = document(server, "execution-profiles", role["execution"])
    if role.get("enabled") is False:
        out(f"WARNING: role `{args.default_role}` is disabled; its tasks are held until it is enabled")
    binding, notes = plan_binding(current, args.default_role, args.default_intelligence, role, execution,
                                  args.raise_ceilings)
    changed = canonical(binding) != canonical(current)
    out(f"project {project} ({binding.get('name')}): binding revision {record['revision']}, "
        f"default role {args.default_role}, default intelligence {args.default_intelligence}, "
        f"execution profile {role['execution']}")
    for note in notes:
        out("  " + note)
    if not changed:
        out("  binding: nothing to change")
    elif args.apply:
        digest = hashlib.sha256(canonical(binding).encode()).hexdigest()
        saved = http(f"{server}/api/projects/{project}", dict(
            operation_id=str(uuid.uuid5(uuid.NAMESPACE_URL, f"bind-project-{project}-{digest}")),
            expected_revision=record["revision"], binding=binding))
        out(f"  binding: saved as revision {(saved or {}).get('revision', record['revision'] + 1)}")
    else:
        out("  binding: would be saved (pass --apply)")

    listed = configured(server, project)
    if listed is not None and not (args.pin or args.branch):
        out(f"  config: [profiled_execution] projects lists the project (enabled = {str(listed).lower()}); "
            "source unchanged")
        if listed is False:
            out("  WARNING: the config entry has enabled = false; the project takes no work until it is true")
        return 0
    repository, generation = binding["repository_id"], binding["repository_binding_generation"]
    target = (binding.get("publication") or {}).get("target")
    branch = args.branch or target or DEFAULT_BRANCH
    git_request(args.stack, repository, generation, "repo.show")
    if not args.apply:
        what = f"follow {branch}" if args.branch else f"pin {args.pin or 'the head of ' + branch}"
        out(f"  source: would sync {branch} in aq-git, retain the starting commit and print the config "
            f"entry ({what}); pass --apply")
        return 0
    tag = hashlib.sha256(canonical([project, args.pin, args.branch, record["revision"]]).encode()).hexdigest()[:16]
    commit, handle = retain(args.stack, repository, generation, branch, args.pin, tag)
    entry = project_entry(project, source_selection(args.pin, args.branch, commit), handle)
    out(f"  source: retained {commit[:12]} under handle {handle}")
    verb = "Add this entry to" if listed is None else "Replace this project's entry in"
    out(f"\n{verb} [profiled_execution] projects in the server's vault config, raise "
        "[profiled_execution] revision by one and restart the server. Keep the priority, placement "
        "and agent_grants of an existing entry.\n")
    out(render_entry(entry))
    return 0


def main(argv=None):
    args = arguments(sys.argv[1:] if argv is None else argv)
    try:
        return run(args)
    except Refused as error:
        print(f"bind-project: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
