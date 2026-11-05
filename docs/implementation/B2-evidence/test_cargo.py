"""Bounded Cargo execution through AQ's pytest resource gate."""
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[3]
COMMANDS = {
    "server_restart_mid_attempt_resumes_or_preserves": ["cargo", "test", "--locked", "--workspace", "server_restart_mid_attempt_resumes_or_preserves"],
    "node_reboot_requires_readoption_before_renewal_extends": ["cargo", "test", "--locked", "--workspace", "node_reboot_requires_readoption_before_renewal_extends"],
    "permit_scope_refuses_foreign_attempt_and_expired_use": ["cargo", "test", "--locked", "--workspace", "permit_scope_refuses_foreign_attempt_and_expired_use"],
    "routes": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "delegated_permit_runtime", "expired_permit_is_refused_by_every_runner_route", "--", "--nocapture"],
    "terminal-quota": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "terminal_store", "sibling_attempts_share_node_terminal_subscription_limit", "--", "--nocapture"],
    "restart": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "delegated_permit_runtime", "server_restart_mid_attempt_resumes_or_preserves", "--", "--nocapture"],
    "receipts-runtime": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "delegated_permit_runtime"],
    "format-includes": ["rustfmt", "--edition", "2024", "crates/aq-server/tests/phase_source_retention/case.rs"],
    "receipts-git": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "phase_source_retention", "delegated_central_receipts", "--", "--ignored", "--nocapture", "--test-threads=1"],
    "receipts-git-node": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "phase_source_retention", "actual_phase_stopped_checkpoint_http_import_and_current_retention_are_not_legacy_gated", "--", "--ignored", "--nocapture", "--test-threads=1"],
    "readoption-final": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "delegated_permit_runtime", "--test", "permit_fixture_conformance", "--test", "native_registration", "--test", "delegated_permits"],
    "registration": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "native_registration", "--test", "delegated_permits"],
    "legacy-registration": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "execution", "execution_http_authenticates_node_binding_and_keeps_routes_separate"],
    "migration-format": ["cargo", "test", "--locked", "-p", "aq-server", "--lib", "state_format_is_the_latest_migration"],
    "migration-merge": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "delegated_permits", "--test", "delegated_permit_runtime", "--test", "central_sessions"],
    "legacy-renewal": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "native_initial_grant", "original_cold_inventory_actual_installation_can_renew_without_retiming_originals"],
    "renewal": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "delegated_permit_runtime", "--test", "phase_refresh_terminal", "--test", "phase_terminal_started"],
    "polls": ["cargo", "test", "--locked", "-p", "aq-server", "--lib", "delegated"],
    "runtime": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "delegated_permit_runtime", "--test", "permit_fixture_conformance"],
    "check": ["cargo", "check", "--locked", "-p", "aq-server"],
    "conformance": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "permit_fixture_conformance"],
    "model": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "delegated_permits"],
    "area": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "native_cancellation"],
    "final": ["cargo", "test", "--locked", "-p", "aq-server", "--test", "delegated_permits", "--test", "native_cancellation", "--", "--test-threads=1"],
    "format-apply": ["cargo", "fmt", "-p", "aq-server"],
    "format": ["cargo", "fmt", "-p", "aq-server", "--check"],
    "clippy": ["cargo", "clippy", "--locked", "-p", "aq-server", "--lib", "--test", "delegated_permits", "--test", "delegated_permit_runtime", "--test", "permit_fixture_conformance", "--test", "phase_source_retention", "--test", "terminal_store", "--", "-D", "warnings"],
}


def test_cargo():
    check = os.environ.get("B2_CHECK", "model")
    environment = os.environ.copy()
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    environment.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    environment.setdefault("CARGO_INCREMENTAL", "0")
    environment.setdefault("RUST_TEST_THREADS", "2")
    log = Path(__file__).with_name(os.environ.get("B2_LOG", check) + ".txt")
    with log.open("w") as output:
        if check.startswith("receipts-git"):
            process = subprocess.Popen(COMMANDS[check], cwd=ROOT, env=environment,
                                       stdout=output, stderr=subprocess.STDOUT,
                                       start_new_session=True)
            process.wait()
            result = process
            remaining = []
            for stat in Path("/proc").glob("[0-9]*/stat"):
                try:
                    fields = stat.read_text().rsplit(") ", 1)[1].split()
                    if int(fields[3]) == process.pid:
                        remaining.append(int(stat.parent.name))
                except (FileNotFoundError, ProcessLookupError):
                    pass
            output.write(f"\nOwned test session {process.pid}; remaining processes: {remaining}\n")
            assert not remaining, "owned test session has remaining processes; preserve private state"
        else:
            result = subprocess.run(COMMANDS[check], cwd=ROOT, env=environment,
                                    stdout=output, stderr=subprocess.STDOUT)
    assert result.returncode == 0, f"{COMMANDS[check]} exited {result.returncode}; see {log}"
