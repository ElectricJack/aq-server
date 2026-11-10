import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("usage", Path(__file__).with_name("refresh-provider-usage.py"))
usage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(usage)


class UsageTests(unittest.TestCase):
    def test_failure_retains_original_time_and_never_claims_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "usage.json"
            with patch.object(usage, "codex", return_value=[usage.window("primary", "Weekly", 52, 1800000000)]), patch.object(usage, "claude", side_effect=ValueError("unavailable")), patch.object(usage.time, "time", return_value=1700000000):
                self.assertEqual(usage.refresh(path, "/tmp", "sdk", "claude", "claude", "codex"), {"anthropic": "unavailable", "openai": "refreshed"})
            original = json.loads(path.read_text())
            with patch.object(usage, "codex", side_effect=ValueError("private error")), patch.object(usage, "claude", side_effect=ValueError("private error")):
                result = usage.refresh(path, "/tmp", "sdk", "claude", "claude", "codex")
            self.assertEqual(json.loads(path.read_text()), original)
            self.assertEqual(result["openai"], "retained stale reading")
            self.assertNotIn("private error", path.read_text())

    def test_invalid_percent_and_reset_are_rejected(self):
        for percent in [-1, 101, float("nan"), float("inf"), True, None]:
            with self.assertRaises(ValueError):
                usage.window("x", "Weekly", percent, None)
        self.assertEqual(usage.window("x", "Weekly", 0, "2026-11-10T00:00:00Z")["reset_at"], 1794268800)

    def test_codex_supported_rpc_selects_codex_bucket_and_actual_window(self):
        class Client:
            def __init__(self, command, cwd): self.command = command
            def request(self, method, params):
                if method == "initialize": return {}
                self.assert_method = method
                if method != "account/rateLimits/read": raise AssertionError(method)
                return {"rateLimits": {"primary": {"usedPercent": 99}}, "rateLimitsByLimitId": {"codex": {"primary": {"usedPercent": 52, "windowDurationMins": 10080, "resetsAt": 1800000000}}}}
            def send(self, method, params): pass
            def close(self): pass
        with patch.object(usage.catalog, "Rpc", Client):
            self.assertEqual(usage.codex("/tmp"), [{"id": "primary", "label": "Weekly", "used_percent": 52, "reset_at": 1800000000}])


if __name__ == "__main__": unittest.main()
