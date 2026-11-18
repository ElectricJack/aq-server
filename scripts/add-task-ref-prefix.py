#!/usr/bin/env python3
"""One-off migration (spec 0016 §5.2): let an existing project's tasks push
`refs/heads/aq/<task-id>` to central aq-git.

  scripts/add-task-ref-prefix.py PROJECT_ID           # show what would change
  scripts/add-task-ref-prefix.py PROJECT_ID --apply   # change it

Repository onboarding registers `allowed_target_prefixes: ["refs/heads/aq/"]`
for new bindings (feature `delivery_playbook`). A binding registered before
that has no prefix. This script adds it:

1. Reads the project's binding from the server (`GET /api/projects/PROJECT_ID`)
   and the repository binding from aq-git (`repo.show`).
2. If `refs/heads/aq/` is missing, asks aq-git for `repo.binding-update`: the
   same binding with the prefix added and the next binding generation. aq-git
   refuses the update while live resources hold the repository
   (`binding_in_use`); the script prints the blockers and changes nothing.
3. Saves the project binding with the new `repository_binding_generation`.
4. Prints the next step: a retention handle belongs to one binding generation,
   so the project's source is retained again with `scripts/bind-project.py`.

Running it again after it succeeded changes nothing. It talks to the server's
owner HTTP API (--server, else AQ_DEV_SERVER_URL) and to aq-git in the stack's
aq-git container (--stack, else COMPOSE_PROJECT_NAME, else aq-ms1), as
`scripts/bind-project.py` does. Without --apply it only reads.
"""
import argparse
import importlib.util
import os
import sys
import uuid
from pathlib import Path

PREFIX = "refs/heads/aq/"
REASON = "spec 0016 §5.2: per-task branches under refs/heads/aq/"
# The complete binding aq-git's `repo.binding-update` replaces.
BINDING_FIELDS = ("repository_id", "binding_generation", "identity", "mode", "object_format",
                  "allowed_refs", "allowed_targets", "allowed_target_prefixes", "remote_url",
                  "credential_profile", "allow_file_transport", "feature_profile")

_spec = importlib.util.spec_from_file_location("bind_project", Path(__file__).with_name("bind-project.py"))
bind = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bind)
Refused = bind.Refused


def plan(shown):
    """The replacement binding, or None when the prefix is already allowed. Pure."""
    binding = {key: shown[key] for key in BINDING_FIELDS if key in shown}
    prefixes = list(binding.get("allowed_target_prefixes") or [])
    if PREFIX in prefixes:
        return None
    if binding.get("mode") != "central":
        raise Refused(f"repository {binding.get('repository_id')} is not a central binding")
    binding["allowed_target_prefixes"] = prefixes + [PREFIX]
    binding["binding_generation"] = shown["binding_generation"] + 1
    return binding


def operation(repository, generation):
    """One durable aq-git operation per repository and starting generation."""
    return str(uuid.uuid5(uuid.NAMESPACE_URL, f"aq-task-ref-prefix-{repository}-{generation}"))


def arguments(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("project", type=lambda text: str(uuid.UUID(text)))
    parser.add_argument("--apply", action="store_true", help="update the aq-git and project bindings")
    parser.add_argument("--server", default=os.environ.get("AQ_DEV_SERVER_URL", "http://127.0.0.1:28080"))
    parser.add_argument("--stack", default=os.environ.get("COMPOSE_PROJECT_NAME", "aq-ms1"))
    return parser.parse_args(argv)


def run(args, out=print):
    server, project = args.server, args.project
    record = bind.http(f"{server}/api/projects/{project}")
    if record is None:
        raise Refused("the server has no binding record for this project")
    current = record["binding"]
    repository, generation = current["repository_id"], current["repository_binding_generation"]
    shown = bind.git_request(args.stack, repository, None, "repo.show")
    replacement = plan(shown)
    out(f"project {project}: repository {repository}, binding generation {generation}")
    if replacement is None and shown["binding_generation"] == generation:
        out(f"  aq-git: {PREFIX} is already allowed; nothing to change")
        return 0
    if replacement is None and shown["binding_generation"] != generation + 1:
        raise Refused(f"aq-git is at binding generation {shown['binding_generation']}, the project at "
                      f"{generation}; reconcile them first")
    if not args.apply:
        out(f"  would bind generation {generation + 1} with {PREFIX} allowed (pass --apply)")
        return 0
    if replacement is not None:
        if shown["binding_generation"] != generation:
            raise Refused(f"aq-git is at binding generation {shown['binding_generation']}, the project at "
                          f"{generation}; reconcile them first")
        try:
            bind.git_request(args.stack, repository, generation, "repo.binding-update",
                             dict(binding=replacement, reason=REASON), mutation=operation(repository, generation))
        except Refused as error:
            if "binding_in_use" in str(error):
                raise Refused(f"aq-git keeps the binding while live resources hold it; release them "
                              f"(finish or cancel the project's attempts) and run this again: {error}") from None
            raise
    # Also resumes a run whose aq-git update succeeded but whose save did not.
    out(f"  aq-git: {PREFIX} allowed at generation {generation + 1}")
    binding = dict(current, repository_binding_generation=generation + 1)
    saved = bind.http(f"{server}/api/projects/{project}", dict(
        operation_id=operation(project, generation), expected_revision=record["revision"], binding=binding))
    out(f"  binding: saved as revision {(saved or {}).get('revision', record['revision'] + 1)}")
    branch = (current.get("publication") or {}).get("target") or bind.DEFAULT_BRANCH
    out(f"\nRetain the project's source again under the new generation:\n"
        f"  scripts/bind-project.py {project} --branch {branch} --apply")
    return 0


def main(argv=None):
    args = arguments(sys.argv[1:] if argv is None else argv)
    try:
        return run(args)
    except Refused as error:
        print(f"add-task-ref-prefix: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
