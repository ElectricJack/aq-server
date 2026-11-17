"""bind-project.py: the parts that need no server (arguments, the binding plan, the config entry)."""
import importlib.util
from pathlib import Path
import tomllib

import pytest

spec = importlib.util.spec_from_file_location("bind_project", Path(__file__).with_name("bind-project.py"))
bind = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bind)

PROJECT = "e0000000-0000-4000-8000-000000000002"
REPOSITORY = "e0000000-0000-4000-8000-000000000003"
ROLE = dict(id="developer", execution="sandbox", enabled=True,
            operations=["task.read", "knowledge.read", "source.write", "wait.register"])
EXECUTION = dict(id="sandbox", environment="aq-sandbox", limits=dict(
    wall_time_ms=900000, memory_bytes=4294967296, cpu_millis=2000, process_limit=512,
    startup_context_bytes=65536, max_output_tokens=32000))


def current(**extra):
    return dict(schema_version=1, project_id=PROJECT, name="Example", enabled=False, repository_id=REPOSITORY,
                repository_binding_generation=1, repository_adapter_reference="aq.git.configured",
                graph_adapter_reference="aq.graph.configured", graph_workspace_id=PROJECT,
                allowed_node_ids=[], allowed_worker_groups=[], allowed_environments=["other"],
                policy=dict(identity="p", revision="1", digest="b" * 64), context=[],
                ceilings=dict(permissions=[dict(operation="task.read", scope_reference=f"project:{PROJECT}")],
                              context_bytes=8192, wall_time_ms=900000, memory_bytes=4294967296, cpu_millis=2000,
                              process_limit=512, cost_limit_microunits=None, currency=None,
                              require_hard_cost_enforcement=False), **extra)


def test_arguments_default_to_a_dry_run_and_refuse_both_sources():
    args = bind.arguments([PROJECT])
    assert (args.apply, args.default_role, args.default_intelligence) == (False, "developer", "standard-high")
    assert bind.arguments([PROJECT, "--branch", "refs/heads/main", "--apply"]).apply
    for bad in (["not-a-uuid"], [PROJECT, "--pin", "abc"], [PROJECT, "--branch", "main"],
                [PROJECT, "--pin", "a" * 40, "--branch", "refs/heads/main"]):
        with pytest.raises(SystemExit):
            bind.arguments(bad)


def test_permissions_take_the_scope_the_server_gives_each_operation():
    assert bind.requested_permissions(ROLE["operations"] + ["task.read"], PROJECT, REPOSITORY) == [
        dict(operation="task.read", scope_reference=f"project:{PROJECT}"),
        dict(operation="knowledge.read", scope_reference=f"project:{PROJECT}"),
        dict(operation="source.write", scope_reference=f"repository:{REPOSITORY}"),
        dict(operation="wait.register", scope_reference="attempt:self")]


def test_the_plan_keeps_what_exists_and_adds_what_the_role_needs():
    old = current(role_notes=dict(developer="Use make."))
    binding, notes = bind.plan_binding(old, "developer", "standard-high", ROLE, EXECUTION)
    assert binding["enabled"] is True and binding["role_notes"] == dict(developer="Use make.")
    # Defaults are left out, as the server leaves them out of what it returns.
    assert "default_role" not in binding and "default_intelligence" not in binding
    assert binding["allowed_environments"] == ["other", "aq-sandbox"]
    assert binding["ceilings"]["permissions"] == bind.requested_permissions(ROLE["operations"], PROJECT, REPOSITORY)
    # A ceiling below the profile's limit is reported, and raised only on request.
    assert binding["ceilings"]["context_bytes"] == 8192
    assert any("context_bytes=8192" in note and "--raise-ceilings" in note for note in notes)
    raised, _ = bind.plan_binding(old, "reviewer", "deep-high", ROLE, EXECUTION, raise_ceilings=True)
    assert raised["ceilings"]["context_bytes"] == 65536
    assert (raised["default_role"], raised["default_intelligence"]) == ("reviewer", "deep-high")
    # Planning again from the result changes nothing.
    again, notes = bind.plan_binding(raised, "reviewer", "deep-high", ROLE, EXECUTION, raise_ceilings=True)
    assert again == raised and notes == []


def test_the_config_entry_is_valid_toml_in_both_spellings():
    for source in (bind.source_selection("a" * 40, None, "b" * 40), bind.source_selection(None, None, "b" * 40),
                   bind.source_selection(None, "refs/heads/main", "b" * 40)):
        entry = bind.project_entry(PROJECT, source, REPOSITORY)
        text = bind.render_entry(entry)
        table, inline = text.split("# or as one element")
        assert tomllib.loads(table)["profiled_execution"]["projects"] == [entry]
        assert tomllib.loads("projects = [" + inline.split("\n", 1)[1] + "]")["projects"] == [entry]
    assert bind.source_selection("a" * 40, None, "b" * 40)["object"]["hex"] == "a" * 40
    assert bind.source_selection(None, None, "b" * 40) == dict(kind="immutable", object=dict(format="sha1", hex="b" * 40))


def test_a_dry_run_reads_only_and_an_unregistered_project_is_refused(monkeypatch):
    calls, shown = [], []

    def http(url, body=None):
        calls.append((url, body))
        if url.endswith("/api/projects/" + PROJECT):
            return dict(revision=3, binding=current())
        if "/api/settings/roles/" in url:
            return dict(fields=ROLE, problem=None)
        if "/api/settings/" in url:
            return dict(fields=EXECUTION, problem=None)
        return dict(items=[], next_cursor=None, execution={})

    git = []
    monkeypatch.setattr(bind, "http", http)
    monkeypatch.setattr(bind, "git_request", lambda *a, **k: git.append((a, k)) or {})
    assert bind.run(bind.arguments([PROJECT, "--server", "http://server"]), out=shown.append) == 0
    assert all(body is None for _, body in calls)
    assert [a[3] for a, k in git] == ["repo.show"] and not any(k.get("mutation") for _, k in git)
    assert any("would be saved" in line for line in shown) and any("would sync" in line for line in shown)
    monkeypatch.setattr(bind, "http", lambda url, body=None: None)
    with pytest.raises(bind.Refused, match="no binding record"):
        bind.run(bind.arguments([PROJECT, "--server", "http://server"]), out=shown.append)
