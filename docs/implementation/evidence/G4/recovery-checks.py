"""Sequential focused validation under aq test admission; no full suite."""
import os
import subprocess
from pathlib import Path


def test_recovery_checks():
    env = os.environ.copy()
    env.update(
        CARGO_BUILD_JOBS="2",
        RUST_TEST_THREADS="2",
        DATABASE_URL="postgres://postgres:test@127.0.0.1:55432/postgres",
    )
    commands = [
        ("recovery-named-overview", ["cargo", "test", "--locked", "--workspace", "overview_attention_lists_waiting_decision_and_failed_task", "--test", "profiled_results"]),
        ("recovery-named-quota", ["cargo", "test", "--locked", "--workspace", "quota_unknown_is_not_zero", "--test", "dashboard_aggregation"]),
        ("recovery-area", ["cargo", "test", "--locked", "-p", "aq-server", "--test", "dashboard_aggregation", "--test", "dashboard_search", "--test", "dashboard_read", "--test", "dashboard_mounts"]),
        ("recovery-overview-area", ["cargo", "test", "--locked", "-p", "aq-server", "--test", "profiled_results", "overview_"]),
        ("recovery-result", ["cargo", "test", "--locked", "-p", "aq-server", "--test", "profiled_results", "dashboard_search_uses_retained_result_metadata_and_digest"]),
        ("recovery-playbook", ["cargo", "test", "--locked", "-p", "aq-server", "--test", "playbook_activation", "dashboard_search_loaded_definition_uses_true_name_and_source_digest"]),
        ("recovery-runtime", ["cargo", "test", "--locked", "-p", "aq-server", "--lib", "dashboard_api::runtime::tests"]),
        ("recovery-quota-collector", ["cargo", "test", "--locked", "-p", "aq-server", "--lib", "dashboard_api::quota::tests"]),
        ("recovery-schema-check", ["/home/jkern/dev/agent-queue2/.venv/bin/python", "protocol/dashboard-read/v1/check.py"]),
        ("recovery-schema-drift", ["cargo", "run", "--locked", "-p", "aq-server", "--example", "dashboard_contract", "--", "--check"]),
        ("recovery-fmt", ["cargo", "fmt", "--check"]),
        ("recovery-clippy", ["cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"]),
    ]
    failures = []
    for name, command in commands:
        result = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        Path(f"docs/implementation/evidence/G4/{name}.log").write_text(result.stdout)
        if result.returncode:
            failures.append((name, result.returncode))
    assert not failures, str(failures)
