"""add-task-ref-prefix.py: the binding plan and the run against fake server and aq-git answers."""
import importlib.util
from pathlib import Path
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("prefix", Path(__file__).with_name("add-task-ref-prefix.py"))
prefix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prefix)

PROJECT = "e0000000-0000-4000-8000-000000000002"
REPOSITORY = "e0000000-0000-4000-8000-000000000003"


def shown(generation=1, **extra):
    return dict(repository_id=REPOSITORY, binding_generation=generation, identity="github.com/acme/widgets",
                mode="central", object_format="sha1", allowed_refs=["refs/heads/main"],
                allowed_targets=["refs/heads/main"], remote_url="ssh://git@github.com/acme/widgets.git",
                observed_at_ms=5, **extra)


def test_plan_adds_the_prefix_at_the_next_generation_and_keeps_the_binding():
    replacement = prefix.plan(shown())
    assert replacement["allowed_target_prefixes"] == ["refs/heads/aq/"]
    assert replacement["binding_generation"] == 2
    assert replacement["allowed_targets"] == ["refs/heads/main"]
    assert "observed_at_ms" not in replacement
    assert prefix.plan(shown(allowed_target_prefixes=["refs/heads/aq/"])) is None


def fake(generations, refuse=None):
    calls = []
    record = dict(revision=3, binding=dict(project_id=PROJECT, repository_id=REPOSITORY,
                                           repository_binding_generation=1))

    def http(url, body=None):
        calls.append(("http", url, body))
        return record if body is None else dict(revision=4)

    def git_request(stack, repository, generation, operation, params=None, mutation=None):
        calls.append(("git", operation, generation, params, mutation))
        if operation == "repo.show":
            return shown(generations.pop(0))
        if refuse:
            raise prefix.Refused(refuse)
        return {}
    prefix.bind.http, prefix.bind.git_request = http, git_request
    return calls


def test_apply_updates_aq_git_then_the_project_binding():
    calls = fake([1])
    lines = []
    assert prefix.run(SimpleNamespace(project=PROJECT, apply=True, server="s", stack="x"), lines.append) == 0
    update = next(c for c in calls if c[0] == "git" and c[1] == "repo.binding-update")
    assert update[2] == 1 and update[3]["binding"]["binding_generation"] == 2
    saved = [c for c in calls if c[0] == "http" and c[2] is not None]
    assert saved[0][2]["binding"]["repository_binding_generation"] == 2
    assert saved[0][2]["expected_revision"] == 3


def test_dry_run_and_live_resources_change_nothing():
    calls = fake([1])
    prefix.run(SimpleNamespace(project=PROJECT, apply=False, server="s", stack="x"), [].append)
    assert not [c for c in calls if c[1] == "repo.binding-update" or (c[0] == "http" and c[2])]
    fake([1], refuse="aq-git refused repo.binding-update: {\"code\": \"binding_in_use\"}")
    try:
        prefix.run(SimpleNamespace(project=PROJECT, apply=True, server="s", stack="x"), [].append)
        raise AssertionError("expected a refusal")
    except prefix.Refused as error:
        assert "live resources" in str(error)


if __name__ == "__main__":
    for name, test in list(globals().items()):
        if name.startswith("test_"):
            test()
    print("ok")
