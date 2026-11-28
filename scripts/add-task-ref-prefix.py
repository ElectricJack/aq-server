#!/usr/bin/env python3
"""Add the task branch prefix to a project's central aq-git binding (spec 0016 §5.2).

  scripts/add-task-ref-prefix.py PROJECT_ID            # show what would change
  scripts/add-task-ref-prefix.py PROJECT_ID --apply    # change it

One-off migration. The server onboards a repository with `allowed_target_prefixes: ["refs/heads/aq/"]`, so each task's
branch `refs/heads/aq/<task-id>` may be pushed to central aq-git and published.
Bindings registered before that lack the prefix. For one project this script:

1. reads the project's server binding (`GET /api/projects/PROJECT_ID`) and its
   aq-git binding (`repo.show`); if the prefix is there, it stops;
2. replaces the aq-git binding with the prefix added (`repo.binding-update`),
   which moves it to the next binding generation. aq-git drains first: it
   refuses while live resources of the old generation block the transition
   and names them, and nothing changes;
3. saves the server binding at the new generation.

Then run `scripts/bind-project.py PROJECT_ID --branch refs/heads/<default> --apply`
to retain a starting commit under the new generation, as after onboarding.

Like bind-project.py it talks to the server's owner HTTP API (--server, else
AQ_DEV_SERVER_URL) and to aq-git in the stack's aq-git container (--stack, else
COMPOSE_PROJECT_NAME, else aq-ms1). Without --apply it only reads.
"""
import argparse
import hashlib
import importlib.util
import os
import sys
import uuid
from pathlib import Path

PREFIX = "refs/heads/aq/"
REASON = "spec 0016: task branches refs/heads/aq/<task-id> are publication targets"


def bind_project():
    """scripts/bind-project.py as a module: its HTTP and aq-git request helpers."""
    spec = importlib.util.spec_from_file_location("bind_project", Path(__file__).with_name("bind-project.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def plan(current):
    """The aq-git binding to replace `current` with, or None when it already has the prefix. Pure."""
    prefixes = list(current.get("allowed_target_prefixes") or [])
    if PREFIX in prefixes:
        return None
    if current.get("mode") != "central":
        raise ValueError("only a central binding takes publication prefixes")
    binding = dict(current)
    binding["allowed_target_prefixes"] = prefixes + [PREFIX]
    binding["binding_generation"] = current["binding_generation"] + 1
    return binding


def run(args, out=print):
    bp = bind_project()
    record = bp.http(f"{args.server}/api/projects/{args.project}")
    if record is None:
        raise bp.Refused("the server has no binding record for this project")
    server_binding = record["binding"]
    repository = server_binding["repository_id"]
    generation = server_binding["repository_binding_generation"]
    current = bp.git_request(args.stack, repository, generation, "repo.show")
    replacement = plan(current)
    if replacement is None:
        out(f"{args.project}: aq-git binding {repository} already allows {PREFIX}; nothing to change")
        return 0
    new_generation = replacement["binding_generation"]
    if not args.apply:
        out(f"{args.project}: would add {PREFIX} to aq-git binding {repository} (generation {generation} -> "
            f"{new_generation}) and save the server binding at {new_generation}; pass --apply")
        return 0
    digest = hashlib.sha256(bp.canonical(replacement).encode()).hexdigest()
    operation = str(uuid.uuid5(uuid.NAMESPACE_URL, f"add-task-ref-prefix-{repository}-{digest}"))
    bp.git_request(args.stack, repository, generation, "repo.binding-update",
                   dict(binding=replacement, reason=REASON), mutation=operation)
    out(f"  aq-git: binding {repository} now allows {PREFIX} at generation {new_generation}")
    saved_binding = dict(server_binding, repository_binding_generation=new_generation)
    digest = hashlib.sha256(bp.canonical(saved_binding).encode()).hexdigest()
    saved = bp.http(f"{args.server}/api/projects/{args.project}", dict(
        operation_id=str(uuid.uuid5(uuid.NAMESPACE_URL, f"add-task-ref-prefix-{args.project}-{digest}")),
        expected_revision=record["revision"], binding=saved_binding))
    out(f"  server: binding saved at generation {new_generation} as revision "
        f"{(saved or {}).get('revision', record['revision'] + 1)}")
    out(f"\nNext: scripts/bind-project.py {args.project} --branch refs/heads/<default> --apply")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("project", type=lambda text: str(uuid.UUID(text)))
    parser.add_argument("--apply", action="store_true", help="replace the aq-git binding and save the server binding")
    parser.add_argument("--server", default=os.environ.get("AQ_DEV_SERVER_URL", "http://127.0.0.1:28080"))
    parser.add_argument("--stack", default=os.environ.get("COMPOSE_PROJECT_NAME", "aq-ms1"))
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    try:
        return run(args)
    except Exception as error:  # Refused, or a refused plan
        print(f"add-task-ref-prefix: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
