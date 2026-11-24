#!/usr/bin/env python3
"""Fetch subscription quota through installed clients. Never sends a model prompt.

Publish only bounded percentages/reset timestamps; failed sources retain their
original observation time. No API keys, OAuth parsing or private HTTP endpoints.
"""
import argparse
import concurrent.futures
from contextlib import closing
import datetime
import importlib.util
import json
import math
from pathlib import Path
import subprocess
import time
import unicodedata

spec = importlib.util.spec_from_file_location("catalog", Path(__file__).with_name("refresh-model-catalog.py"))
catalog = importlib.util.module_from_spec(spec)
spec.loader.exec_module(catalog)


def bounded_integer(value, lower, upper):
    return isinstance(value, int) and not isinstance(value, bool) and lower <= value <= upper


def usage_percentage(value):
    return (not isinstance(value, bool) and isinstance(value, (int, float))
            and math.isfinite(value) and 0 <= value <= 100)


def window(identity, label, percent, reset):
    if not usage_percentage(percent):
        raise ValueError("unknown percentage")
    if isinstance(reset, str):
        reset = int(datetime.datetime.fromisoformat(reset.replace("Z", "+00:00")).timestamp())
    if reset is not None and not bounded_integer(reset, 1, math.inf):
        raise ValueError("invalid reset")
    return dict(id=identity, label=label, used_percent=percent, reset_at=reset)


def reset_unknown():
    return dict(state="unknown", available_count=None, grants=None)


def valid_label(label):
    if not isinstance(label, str) or not label.strip():
        return False
    try:
        return len(label.encode("utf-8")) <= 120 and all(unicodedata.category(c) != "Cc" for c in label)
    except UnicodeEncodeError:
        return False


def codex_resets(value):
    """Normalize documented app-server reset credits without copying opaque IDs."""
    block = value.get("rateLimitResetCredits")
    if not isinstance(block, dict):
        return reset_unknown()
    count = block.get("availableCount")
    if not bounded_integer(count, 0, 1_000_000):
        return reset_unknown()
    details = block.get("credits")
    grants = None
    if isinstance(details, list) and len(details) <= 100:
        grants = [grant for row in details if (grant := available_grant(row)) is not None]
    return dict(state="known", available_count=count, grants=grants)


def available_grant(row):
    if not isinstance(row, dict) or row.get("status") != "available":
        return None
    label = row.get("title")
    if label is None:
        label = "Rate-limit reset"
    if not valid_label(label):
        return None
    try:
        expires_at = grant_expiry(row.get("expiresAt"))
    except (ValueError, OSError, OverflowError):
        return None
    # Availability alone does not establish which windows can be reset now.
    return dict(label=label, remaining_count=None, expires_at=expires_at, usable_now=None, clears=[])


def grant_expiry(expiry):
    if expiry is None:
        return None
    if not bounded_integer(expiry, 1, 253402300799):
        raise ValueError("invalid grant expiry")
    timestamp = datetime.datetime.fromtimestamp(expiry, datetime.timezone.utc)
    return timestamp.isoformat(timespec="milliseconds").replace("+00:00", "Z")


def codex(cwd):
    with closing(catalog.Rpc(["codex", "app-server", "--stdio"], cwd)) as client:
        catalog.initialize_codex(client, "aq_provider_usage")
        value = client.request("account/rateLimits/read", {})
        rate = (value.get("rateLimitsByLimitId") or {}).get("codex") or value["rateLimits"]
        return codex_windows(rate), codex_resets(value)


def codex_windows(rate):
    windows = []
    for key in ("primary", "secondary"):
        row = rate.get(key)
        if row:
            label = duration_label(key, row.get("windowDurationMins"))
            windows.append(window(key, label, row["usedPercent"], row.get("resetsAt")))
    return windows


def duration_label(key, minutes):
    standard = {300: "5 hours", 10080: "Weekly", 1440: "Daily"}
    fallback = f"{minutes} minutes" if isinstance(minutes, int) and 0 < minutes < 100000 else key.title()
    return standard.get(minutes, fallback)


def claude(cwd, sdk, executable):
    value = claude_usage_response(cwd, sdk, executable)
    if value.get("rate_limits_available") is not True or not isinstance(value.get("rate_limits"), dict):
        raise ValueError("subscription usage unavailable")
    # The documented experimental SDK control exposes windows, not grants.
    return claude_windows(value["rate_limits"]), reset_unknown()


def claude_usage_response(cwd, sdk, executable):
    helper = Path(__file__).with_name("provider-usage-claude.mjs")
    result = subprocess.run(["node", str(helper), sdk, executable], cwd=cwd, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, timeout=35, check=True)
    if len(result.stdout) > 65536:
        raise ValueError("usage response too large")
    return json.loads(result.stdout)


def claude_windows(limits):
    windows = []
    for key, label in [("five_hour", "5 hours"), ("seven_day", "Weekly"), ("seven_day_opus", "Weekly Opus"), ("seven_day_sonnet", "Weekly Sonnet"), ("seven_day_oauth_apps", "Weekly OAuth apps")]:
        row = limits.get(key)
        if isinstance(row, dict) and row.get("utilization") is not None:
            windows.append(window(key, label, row["utilization"], row.get("resets_at")))
    return windows


def cached_claude(path, expected_account):
    """Read CLI-produced quota only, bound to an explicit operator account ID.

    Preserve the provider observation time: reading an old cache is never a
    fresh quota request. No credentials, session costs or token sums are read.
    """
    if not expected_account or not readable_cache(path):
        return None
    value = json.loads(path.read_text()).get("cachedUsageUtilization", {})
    observed = value.get("fetchedAtMs")
    if value.get("accountUuid") != expected_account or not bounded_integer(observed, 1, int(time.time()*1000)+5000):
        return None
    windows = claude_windows(value.get("utilization", {}))
    return (observed, windows) if windows else None


def readable_cache(path):
    if path is None or path.is_symlink():
        return False
    meta = path.stat()
    return path.is_file() and not (meta.st_mode & 0o022) and meta.st_size <= 4_194_304


def refresh(settings):
    retained = last_readings(settings.output)
    accounts, statuses = [], {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        jobs = [(p, a, pool.submit(fetch)) for p, a, fetch in usage_sources(settings)]
        for provider, account, job in jobs:
            try:
                accounts.append(fresh_reading(provider, account, job.result()))
                statuses[provider] = "refreshed"
            except Exception:
                previous = retained.get((provider, account))
                if provider == "anthropic":
                    previous = newest_claude_reading(previous, account, settings.claude_cache, settings.claude_account_id)
                if previous:
                    accounts.append(previous)
                statuses[provider] = "retained stale reading" if previous else "unavailable"
    catalog.atomic_json(settings.output, dict(schema_version=1, accounts=accounts), mode=0o644)
    return statuses


def usage_sources(settings):
    return [("anthropic", settings.claude_account, lambda: claude(settings.cwd, settings.claude_sdk, settings.claude)),
               ("openai", settings.codex_account, lambda: codex(settings.cwd))]


def last_readings(output):
    try:
        old = json.loads(output.read_text())
        return {(a["provider"], a["account_ref"]): a for a in old["accounts"]}
    except (OSError, ValueError, KeyError, TypeError):
        return {}


def fresh_reading(provider, account, reading):
    windows, resets = reading
    if not 1 <= len(windows) <= 16:
        raise ValueError("no windows")
    observed = int(time.time()*1000)
    resets["observed_at_ms"] = observed
    return dict(provider=provider, account_ref=account, observed_at_ms=observed, windows=windows, reset_credits=resets)


def newest_claude_reading(previous, account, cache, account_id):
    try:
        cached = cached_claude(cache, account_id)
        return merge_cached_reading(previous, account, cached)
    except (OSError, ValueError, KeyError, TypeError):
        return previous


def merge_cached_reading(previous, account, cached):
    if not cached:
        return previous
    if previous is not None and cached[0] <= previous["observed_at_ms"]:
        return previous
    reading = dict(provider="anthropic", account_ref=account, observed_at_ms=cached[0],
                   windows=cached[1], source="claude_cli_usage_cache")
    if previous and previous.get("reset_credits") is not None:
        reading["reset_credits"] = previous["reset_credits"]
    return reading


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cwd", default="/tmp")
    parser.add_argument("--claude-sdk", required=True)
    parser.add_argument("--claude", default="claude")
    parser.add_argument("--claude-account", default="owner-claude-subscription")
    parser.add_argument("--codex-account", default="Codex ChatGPT")
    parser.add_argument("--claude-cache", type=Path, help="Optional Claude CLI quota cache; observation timestamps are retained")
    parser.add_argument("--claude-account-id", help="Explicit account identity required for the optional quota cache")
    args = parser.parse_args()
    print(json.dumps(refresh(args)))


if __name__ == "__main__":
    main()
