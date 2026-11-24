#!/usr/bin/env python3
"""Refresh picker metadata without prompts, inference, API keys, or configuration edits.

Official subscription clients supply account model lists. The public OpenCode
catalog supplies remaining model/effort metadata, explicitly without access claims.
Failed sources retain their last snapshot; output replacement is atomic.
"""
import argparse
import concurrent.futures
from contextlib import closing
from dataclasses import dataclass
import datetime
import json
import os
from pathlib import Path
import selectors
import subprocess
import tempfile
import time
import urllib.request


def fetch(url, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers={"Accept": "application/json", "Content-Type": "application/json", "User-Agent": "aq-model-catalog/1"})
    with urllib.request.urlopen(req, timeout=20) as response:
        raw = response.read(12 * 1024 * 1024 + 1)
    if len(raw) > 12 * 1024 * 1024:
        raise ValueError("metadata response too large")
    return json.loads(raw)


class Rpc:
    _INVALID_MESSAGE = object()

    def __init__(self, command, cwd):
        env = {k: v for k, v in os.environ.items() if k not in {"OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GOOGLE_API_KEY", "GEMINI_API_KEY"}}
        self.process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.identity = 0

    def send(self, method, params, identity=None):
        message = {"jsonrpc": "2.0", "method": method, "params": params}
        if identity is not None:
            message["id"] = identity
        self.process.stdin.write(json.dumps(message).encode() + b"\n")
        self.process.stdin.flush()

    def request(self, method, params):
        self.identity += 1
        self.send(method, params, self.identity)
        deadline = time.monotonic() + 30
        for value in self.responses(deadline):
            if value.get("id") == self.identity:
                if "error" in value:
                    raise RuntimeError("metadata client refused discovery")
                return value["result"]
        raise TimeoutError("metadata client timed out")

    def responses(self, deadline):
        while time.monotonic() < deadline:
            value = self.next_message()
            if value is not self._INVALID_MESSAGE:
                yield value

    def read_available(self):
        if not self.selector.select(1):
            return
        chunk = os.read(self.process.stdout.fileno(), 65536)
        if not chunk:
            raise RuntimeError("metadata client exited")
        self.buffer += chunk
        if len(self.buffer) > 2 * 1024 * 1024:
            raise ValueError("metadata client response too large")

    def next_message(self):
        if b"\n" not in self.buffer:
            self.read_available()
        if b"\n" not in self.buffer:
            return self._INVALID_MESSAGE
        line, self.buffer = self.buffer.split(b"\n", 1)
        try:
            return json.loads(line)
        except ValueError:
            return self._INVALID_MESSAGE

    def close(self):
        self.selector.close()
        self.process.terminate()
        try:
            self.process.wait(5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def initialize_codex(client, name):
    client.request("initialize", {"clientInfo": {"name": name, "version": "1"}, "capabilities": {}})
    client.send("initialized", {})


def codex_models(cwd):
    with closing(Rpc(["codex", "app-server", "--stdio"], cwd)) as client:
        initialize_codex(client, "aq_model_catalog")
        models, cursor = [], None
        for _ in range(32):
            result = client.request("model/list", {"limit": 100, "includeHidden": True, "cursor": cursor})
            models.extend(result["data"])
            cursor = result.get("nextCursor")
            if not cursor:
                return models
        raise ValueError("too many model pages")


def gemini_models(cwd):
    with closing(Rpc(["gemini", "--acp"], cwd)) as client:
        client.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "aq-model-catalog", "version": "1"}})
        return client.request("session/new", {"cwd": cwd, "mcpServers": []})["models"]["availableModels"]


@dataclass(frozen=True)
class CatalogSource:
    provider: str
    harness: str
    description: str
    observed_at: str | None

    def choice(self, model, effort, options):
        return dict(provider=self.provider, model=model, reasoning=effort, options=options,
                    harness=self.harness, source=self.description, observed_at=self.observed_at)


def suggestions(choices):
    """The cache the server reads for GET /api/settings/model-suggestions: one
    {provider, model, harness, source, observed_at} per harness, provider and model.
    The first choice of a model wins, so official client facts stay ahead of public
    metadata. Reasoning levels are not suggestions: a level document states its own."""
    unique = {}
    for c in choices:
        entry = {"provider": c["provider"], "model": c["model"], "harness": c["harness"]}
        for key in ("source", "observed_at"):
            if c.get(key) is not None:
                entry[key] = c[key]
        unique.setdefault((c["harness"], c["provider"], c["model"]), entry)
    return sorted(unique.values(), key=lambda e: (e["harness"], e["provider"], e["model"]))


def public_choices(catalog, observed):
    result = []
    providers = [("anthropic", "anthropic", "claude"), ("openai", "openai", "codex"), ("google", "google", "gemini"), ("opencode", "opencode", "opencode"), ("longcat", "opencode", "opencode"), ("nemotron", "opencode", "opencode")]
    for provider, upstream, harness in providers:
        for identity, model in catalog.get(upstream, {}).get("models", {}).items():
            if not eligible_public_model(provider, identity, model):
                continue
            name = "opencode/" + identity if upstream == "opencode" else identity
            source = CatalogSource(provider, harness, "OpenCode public catalog (access unverified)", observed)
            controls = [option for control in model.get("reasoning_options", [])
                        for option in reasoning_options(provider, control)]
            for effort, options in controls or [("Provider default", {})]:
                result.append(source.choice(name, effort, options))
    return result


def eligible_public_model(provider, identity, model):
    if not model.get("tool_call") or "text" not in model.get("modalities", {}).get("input", ["text"]):
        return False
    return provider not in {"longcat", "nemotron"} or provider in identity.lower()


def reasoning_options(provider, control):
    if control["type"] == "effort":
        key = {"anthropic": "thinking", "google": "thinking_level"}.get(provider, "reasoning_effort")
        return [(level, {key: level}) for level in control.get("values", [])]
    if control["type"] == "budget_tokens":
        budgets = {control[k] for k in ("min", "max") if isinstance(control.get(k), int)}
        return [(f"Budget {budget:,}", {"thinking_budget": budget}) for budget in sorted(budgets)]
    if control["type"] == "toggle" and provider not in {"anthropic", "google"}:
        return [("Thinking on", {"think": True}), ("Thinking off", {"think": False})]
    return []


def atomic_json(path, value, mode=0o600):
    path.parent.mkdir(parents=True, exist_ok=True)
    raw = (json.dumps(value, indent=2) + "\n").encode()
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as temporary:
        os.fchmod(temporary.fileno(), mode)
        temporary.write(raw)
        temporary.flush()
        os.fsync(temporary.fileno())
    os.replace(temporary.name, path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--claude-sdk", required=True, type=Path, help="Installed official SDK's sdk.mjs")
    parser.add_argument("--ollama-url", default="http://127.0.0.1:11434")
    args = parser.parse_args()
    observed = datetime.datetime.now(datetime.timezone.utc).isoformat()
    state_path = args.output.with_name("model-catalog-sources.json")
    previous = json.loads(state_path.read_text()) if state_path.exists() else {}
    discover(previous, observed, args)
    choices = catalog_choices(previous)
    imported = args.output.with_name("imported-model-choices.json")
    if imported.exists():
        choices.extend(json.loads(imported.read_text()))
    result = suggestions(choices)
    if not fits_settings_cache(result):
        raise ValueError("catalog empty or exceeds Settings limits; old cache preserved")
    atomic_json(state_path, previous)
    atomic_json(args.output, result, 0o644)
    print(json.dumps(refresh_summary(result, previous)))


def refresh_summary(choices, sources):
    return {
        "suggestions": len(choices),
        "models": len({(c["provider"], c["model"]) for c in choices}),
        "providers": sorted({c["provider"] for c in choices}),
        "sources": {k: v["status"] for k, v in sources.items()},
    }


def fits_settings_cache(result):
    return 0 < len(result) <= 4096 and len(json.dumps(result).encode()) <= 2_097_152


def discover(previous, observed, args):
    with tempfile.TemporaryDirectory(prefix="aq-model-metadata-") as cwd:
        jobs = {
            "codex": lambda: codex_models(cwd),
            "claude": lambda: claude_models(args.claude_sdk, cwd),
            "gemini": lambda: gemini_models(cwd),
            "public": lambda: fetch("https://models.opencode.ai/api.json"),
            "ollama": lambda: ollama_models(args.ollama_url),
        }
        collect_sources(previous, observed, jobs)


def claude_models(sdk, cwd):
    helper = Path(__file__).with_name("model-catalog-claude.mjs")
    output = subprocess.check_output(["node", str(helper), str(sdk.resolve()), "claude"],
                                     cwd=cwd, stderr=subprocess.DEVNULL, timeout=35)
    return json.loads(output)


def ollama_models(url):
    url = url.rstrip("/")
    return [{**model, "details": fetch(url + "/api/show", {"model": model["name"]})}
            for model in fetch(url + "/api/tags")["models"]]


def collect_sources(previous, observed, jobs):
    with concurrent.futures.ThreadPoolExecutor(max_workers=5) as executor:
        pending = {executor.submit(job): name for name, job in jobs.items()}
        for future in concurrent.futures.as_completed(pending):
            name = pending[future]
            try:
                previous[name] = {"observed_at": observed, "data": future.result(), "status": "fresh"}
            except Exception as error:
                previous.setdefault(name, {"data": [], "observed_at": None})["status"] = "unavailable: " + type(error).__name__


def catalog_choices(previous):
    public = previous.get("public", {})
    choices = public_choices(public.get("data") or {}, public.get("observed_at"))
    append_codex(choices, previous)
    append_claude(choices, previous)
    append_gemini(choices, previous)
    append_ollama(choices, previous)
    for cached in choices:
        key = next((key for prefix, key in [("Codex", "codex"), ("Claude", "claude"), ("Gemini", "gemini"), ("Local Ollama", "ollama"), ("OpenCode", "public")] if cached["source"].startswith(prefix)), None)
        if key and previous.get(key, {}).get("status") != "fresh":
            cached["source"] += " (last known; refresh unavailable)"
    return choices


def append_codex(choices, previous):
    for model in previous.get("codex", {}).get("data", []):
        source = CatalogSource("openai", "codex", "Codex account catalog", previous["codex"]["observed_at"])
        levels = [(e["reasoningEffort"], {"reasoning_effort": e["reasoningEffort"]}) for e in model.get("supportedReasoningEfforts", [])]
        for level, options in levels or [("Provider default (levels not reported)", {})]:
            choices.append(source.choice(model["model"], level, options))


def append_claude(choices, previous):
    for model in previous.get("claude", {}).get("data", []):
        source = CatalogSource("anthropic", "claude", "Claude Code account catalog", previous["claude"]["observed_at"])
        name = model.get("resolvedModel", model["value"]) if model["value"] in {"default", "opus", "sonnet", "haiku"} else model["value"]
        levels = [(level, {"thinking": level}) for level in model.get("supportedEffortLevels", [])]
        for level, options in levels or [("Provider default (levels not reported)", {})]:
            choices.append(source.choice(name, level, options))


def append_gemini(choices, previous):
    for model in previous.get("gemini", {}).get("data", []):
        source = CatalogSource("google", "gemini", "Gemini CLI account catalog", previous["gemini"]["observed_at"])
        name = model["modelId"]
        existing = [c for c in choices if c["provider"] == "google" and c["model"] == name]
        for c in existing or [source.choice(name, "Provider default (levels not reported)", {})]:
            c = dict(c, source="Gemini CLI account catalog" + ("; effort metadata: OpenCode" if existing else ""))
            choices.append(c)


def append_ollama(choices, previous):
    for model in previous.get("ollama", {}).get("data", []):
        caps = model["details"].get("capabilities", [])
        if "completion" not in caps:
            continue
        source = CatalogSource("ollama", "opencode", "Local Ollama API", previous["ollama"]["observed_at"])
        values = model["details"].get("thinking", {}).get("values", [])
        levels = [("Thinking on" if value is True else "Thinking off" if value is False else str(value), {"think": value}) for value in values if isinstance(value, (str, bool))]
        if not levels:
            levels = [("Provider default (levels not reported)", {})]
        for level, options in levels:
            choices.append(source.choice("ollama/" + model["name"], level, options))


if __name__ == "__main__":
    main()
