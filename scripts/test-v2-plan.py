#!/usr/bin/env python3
"""Regression checks for frozen planning provenance; run with aq test."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import tempfile
import unittest
from unittest.mock import patch

import yaml

spec = importlib.util.spec_from_file_location("v2_plan", Path(__file__).with_name("check-v2-plan.py"))
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)
ROOT = checker.ROOT
AUTHORITY = "docs/superpowers/plans/2026-11-02-v2-full-application-plan.md"
INDEX = ROOT / "docs/plans/requirement-sources.md"


class PlanProvenanceTests(unittest.TestCase):
    def test_frozen_header_and_generated_document(self):
        plan = json.loads(checker.SOURCE.read_text())
        self.assertEqual(plan["tracking_authority"], AUTHORITY)
        self.assertIn("Frozen", plan["freeze_note"])
        self.assertEqual(hashlib.sha256(json.dumps(plan["tasks"], sort_keys=True).encode()).hexdigest(),
                         "cbfc409d34fd909fd0dfeb8e35eb7a0426923c85461348f0736ca02033c9e8e8")
        rendered = checker.render(plan)
        self.assertIn("Frozen", rendered[:1100])
        self.assertIn(AUTHORITY.removeprefix("docs/"), rendered[:1100])
        self.assertEqual(checker.OUTPUT.read_text(), rendered)

    def test_historical_owners_and_worktree_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            checkout = base / "aq-server"
            slot = checkout / ".aq/worktrees/slot-6"
            gitdir = checkout / ".git/worktrees/slot-6"
            slot.mkdir(parents=True)
            gitdir.mkdir(parents=True)
            (slot / ".git").write_text(f"gitdir: {gitdir}\n")
            (gitdir / "commondir").write_text("../..\n")
            self.assertEqual(checker.checkout_root(slot), checkout)
            self.assertEqual(checker.checkout_root(checkout), checkout)
            with patch.object(checker, "ROOT", slot), patch.object(checker, "CHECKOUT_ROOT", checkout), patch.object(checker, "SOURCE", slot / "docs/plans/old.json"), patch.dict(os.environ, {"AQ_PLAN_PLATFORM_ROOT": str(base)}):
                self.assertEqual(checker.owner_directory("agent-q"), slot)
                self.assertEqual(checker.owner_directory("aq-worker"), base / "aq-worker")
                self.assertEqual(checker.plan_path("../../../aq-worker/docs/specs/a.md"), base / "aq-worker/docs/specs/a.md")
                self.assertEqual(checker.plan_path("../implementation/A1.md"), slot / "docs/plans/../implementation/A1.md")
                reference = base / "reference"
                with patch.dict(os.environ, {"AQ_PLAN_PLATFORM_ROOT": str(reference)}):
                    self.assertEqual(checker.owner_directory("aq-worker"), reference / "aq-worker")
                    self.assertEqual(checker.owner_directory("agent-q"), slot)
        checker.read_plan()

    def test_complete_successor_mapping(self):
        old = json.loads(checker.SOURCE.read_text())
        tasks = yaml.safe_load((ROOT / AUTHORITY.replace("-plan.md", "-tasks.yaml")).read_text())["tasks"]
        rows = re.findall(r"^\| ([A-Z]+-\d{2}) \| ([^|]+) \| ([^|]+) \|$", INDEX.read_text(), re.M)
        expected = [t for t in old["tasks"] if t["status"] in {"in_progress", "planned"}]
        self.assertEqual(len(rows), 66)
        self.assertEqual([r[0] for r in rows], [t["id"] for t in expected])
        by_id = {task["id"]: task for task in expected}
        for old_id, status, successors in rows:
            self.assertEqual(status.strip(), by_id[old_id]["status"])
            self.check_successors(old_id, successors, tasks)
        for old_id in ["API-04", "UI-07"]:
            self.assertIn("No successor", next(r[2] for r in rows if r[0] == old_id))

    def check_successors(self, old_id, successors, tasks):
        matches = [task for task in tasks if old_id in task.get("continues", [])]
        self.assertEqual(re.findall(r"`([^`]+)`", successors), [task["key"] for task in matches])
        for task in matches:
            if task.get("deferred"):
                self.assertIn(f"`{task['key']}` (deferred)", successors)
        if not matches:
            self.assertIn("No successor", successors)

    def test_unique_requirement_sources(self):
        text = INDEX.read_text()
        rows = re.findall(r"^\| `([A-Z][A-Z0-9-]*)` \| ([^|]+) \| ([^|]+) \|$", text, re.M)
        prefixes = [r[0] for r in rows]
        self.assertEqual(len(prefixes), len(set(prefixes)))
        self.assertEqual(set(prefixes), {"E", "X", "D", "W", "U", "P", "H", "R", "S", "N", "C", "SBX", "DEP", "GIT-R", "GIT-V", "GIT-P", "PB-P", "PB-T", "WRK-T", "GRAPH-1", "GRAPH-2", "GRAPH-3", "GRAPH-4", "SRV-INFRA", "SRV-BIND", "SRV-INTEL", "SRV-MEDIA", "UI-PRODUCT", "UI-API", "WRK-PROFILE", "WRK-OBS"})
        for _, sources, _ in rows:
            for target in re.findall(r"\]\(([^)]+)\)", sources):
                self.assertTrue(checker.plan_path(target).is_file(), target)
        self.assertIn("section", text)


if __name__ == "__main__":
    unittest.main()
