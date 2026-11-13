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
    def test_claude_usage_windows_do_not_claim_a_reset_count(self):
        result = type("Result", (), {"stdout": json.dumps({"rate_limits_available": True, "rate_limits": {
            "seven_day": {"utilization": 70, "resets_at": "2026-12-01T00:00:00Z"},
            "cedar_ember": {"grants": [{"resets_left": 5}]}}}).encode()})()
        with patch.object(usage.subprocess, "run", return_value=result):
            windows, resets = usage.claude("/tmp", "sdk", "claude")
        self.assertEqual(windows[0]["used_percent"], 70)
        self.assertEqual(resets, usage.reset_unknown())

    def test_codex_reset_count_and_details_are_independent(self):
        norm = usage.codex_resets
        self.assertEqual(norm({}), usage.reset_unknown())
        for count in [None, -1, True, 1.5, 1000001, "0"]:
            self.assertEqual(norm({"rateLimitResetCredits": {"availableCount": count}}), usage.reset_unknown())
        self.assertEqual(norm({"rateLimitResetCredits": {"availableCount": 0, "credits": []}}),
                         {"state": "known", "available_count": 0, "grants": []})
        self.assertIsNone(norm({"rateLimitResetCredits": {"availableCount": 7, "credits": None}})["grants"])
        value = norm({"rateLimitResetCredits": {"availableCount": 7, "credits": [
            {"id": "secret", "title": "Weekly reset", "expiresAt": 1800000000, "status": "available"},
            {"title": "bad", "expiresAt": -1}, {"title": "x" * 121}, None]}})
        self.assertEqual(value["available_count"], 7)
        self.assertEqual(value["grants"], [{"label": "Weekly reset", "remaining_count": None,
            "expires_at": "2027-01-15T08:00:00.000Z", "usable_now": None, "clears": []}])
        self.assertNotIn("secret", json.dumps(value))
        self.assertIsNone(norm({"rateLimitResetCredits": {"availableCount": 7, "credits": [{}]*101}})["grants"])

    def test_reset_details_exclude_spent_or_unknown_status_without_changing_count(self):
        result = usage.codex_resets({"rateLimitResetCredits": {
            "availableCount": 3,
            "credits": [
                {"title": "Available reset", "status": "available"},
                {"title": "Already spent reset", "status": "redeemed"},
                {"title": "Unqualified reset"},
            ],
        }})
        self.assertEqual(result["available_count"], 3)
        self.assertEqual([grant["label"] for grant in result["grants"]], ["Available reset"])

    def test_labels_match_server_utf8_bounds_and_control_validation(self):
        labels = ["週" * 40, "週" * 41, "bad\x7f", "bad\u0085", "bad\ud800", "Good reset"]
        result = usage.codex_resets({"rateLimitResetCredits": {"availableCount": 8,
            "credits": [{"title": label, "status": "available"} for label in labels]}})
        self.assertEqual(result["available_count"], 8)
        self.assertEqual([row["label"] for row in result["grants"]], [labels[0], labels[-1]])

    def test_successful_omission_clears_count_and_failure_retains_it(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "usage.json"
            windows = [usage.window("primary", "Weekly", 52, None)]
            known = {"state": "known", "available_count": 2, "grants": None}
            with patch.object(usage, "claude", side_effect=ValueError()), patch.object(usage, "codex", return_value=(windows, known)), patch.object(usage.time, "time", return_value=1700000000):
                usage.refresh(path, "/tmp", "sdk", "claude", "owner", "codex")
            with patch.object(usage, "claude", side_effect=ValueError()), patch.object(usage, "codex", side_effect=ValueError()), patch.object(usage.time, "time", return_value=1700000050):
                usage.refresh(path, "/tmp", "sdk", "claude", "owner", "codex")
            self.assertEqual(json.loads(path.read_text())["accounts"][0]["reset_credits"]["observed_at_ms"], 1700000000000)
            with patch.object(usage, "claude", side_effect=ValueError()), patch.object(usage, "codex", return_value=(windows, usage.reset_unknown())), patch.object(usage.time, "time", return_value=1700000060):
                usage.refresh(path, "/tmp", "sdk", "claude", "owner", "codex")
            self.assertEqual(json.loads(path.read_text())["accounts"][0]["reset_credits"],
                             {"state": "unknown", "available_count": None, "grants": None, "observed_at_ms": 1700000060000})

    def test_cache_preserves_reset_observation_independently_and_account_binding(self):
        with tempfile.TemporaryDirectory() as directory:
            path, cache = Path(directory) / "usage.json", Path(directory) / "cache.json"
            reset = {"state": "known", "available_count": 1, "grants": None, "observed_at_ms": 1600000000000}
            old = {"provider": "anthropic", "account_ref": "owner", "observed_at_ms": 1600000000000, "windows": [], "reset_credits": reset}
            path.write_text(json.dumps({"accounts": [old]}))
            cache.write_text(json.dumps({"cachedUsageUtilization": {"accountUuid": "owner-id", "fetchedAtMs": 1700000000000, "utilization": {"seven_day": {"utilization": 20}}}}))
            with patch.object(usage, "claude", side_effect=ValueError()), patch.object(usage, "codex", side_effect=ValueError()), patch.object(usage.time, "time", return_value=1800000000):
                usage.refresh(path, "/tmp", "sdk", "claude", "owner", "codex", cache, "owner-id")
                account = json.loads(path.read_text())["accounts"][0]
                self.assertEqual(account["observed_at_ms"], 1700000000000)
                self.assertEqual(account["reset_credits"], reset)
                usage.refresh(path, "/tmp", "sdk", "claude", "different", "codex", cache, "wrong-id")
                self.assertEqual(json.loads(path.read_text())["accounts"], [])

    def test_failure_retains_original_time_and_never_claims_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "usage.json"
            with patch.object(usage, "codex", return_value=([usage.window("primary", "Weekly", 52, 1800000000)], usage.reset_unknown())), patch.object(usage, "claude", side_effect=ValueError("unavailable")), patch.object(usage.time, "time", return_value=1700000000):
                self.assertEqual(usage.refresh(path, "/tmp", "sdk", "claude", "claude", "codex"), {"anthropic": "unavailable", "openai": "refreshed"})
            original = json.loads(path.read_text())
            with patch.object(usage, "codex", side_effect=ValueError("private error")), patch.object(usage, "claude", side_effect=ValueError("private error")):
                result = usage.refresh(path, "/tmp", "sdk", "claude", "claude", "codex")
            self.assertEqual(json.loads(path.read_text()), original)
            self.assertEqual(result["openai"], "retained stale reading")
            self.assertNotIn("private error", path.read_text())

    def test_cli_quota_cache_keeps_timestamp_and_rejects_another_account(self):
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory) / "claude.json"
            cache.write_text(json.dumps({"cachedUsageUtilization": {"accountUuid": "owner", "fetchedAtMs": 1700000000000, "utilization": {"seven_day": {"utilization": 94, "resets_at": "2026-11-11T22:00:00Z"}}}}))
            with patch.object(usage.time, "time", return_value=1800000000):
                self.assertEqual(usage.cached_claude(cache, "owner")[0], 1700000000000)
                self.assertEqual(usage.cached_claude(cache, "owner")[1][0]["used_percent"], 94)
                self.assertIsNone(usage.cached_claude(cache, "other"))
                self.assertIsNone(usage.cached_claude(cache, None))
            output = Path(directory) / "usage.json"
            with patch.object(usage, "claude", side_effect=ValueError()), patch.object(usage, "codex", return_value=([usage.window("primary", "Weekly", 52, None)], usage.reset_unknown())):
                status = usage.refresh(output, "/tmp", "sdk", "claude", "owner-claude", "codex", cache, "owner")
                self.assertEqual(status["anthropic"], "retained stale reading")
                self.assertEqual(json.loads(output.read_text())["accounts"][0]["observed_at_ms"], 1700000000000)

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
            self.assertEqual(usage.codex("/tmp"), ([{"id": "primary", "label": "Weekly", "used_percent": 52, "reset_at": 1800000000}], usage.reset_unknown()))


if __name__ == "__main__": unittest.main()
