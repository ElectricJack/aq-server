#!/usr/bin/env python3
"""Regression checks for project-relative imported task instructions; run with aq test."""
import importlib.util
import os
from pathlib import Path
import re
import subprocess
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("import_v1_plan", ROOT / "scripts/import-v1-plan.py")
importer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(importer)


class ImportedTaskProjectTests(unittest.TestCase):
    def test_every_rendered_description_uses_the_worker_project(self):
        source = ROOT / "docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml"
        tasks = yaml.safe_load(source.read_text(encoding="utf-8"))["tasks"]
        self.assertTrue(tasks)
        self.assertEqual({task["repo"] for task in tasks}, set(importer.REPO_KIND))
        for task in tasks:
            with self.subTest(key=task["key"], repo=task["repo"]):
                rendered = importer.description(task)
                self.assertNotIn("--project aq-platform", rendered)
                self.assertIn('--project "$AQ_PROJECT_ID"', rendered)

    def test_footer_command_expands_the_current_project_at_execution(self):
        footer = importer.FOOTER.format(plan=importer.PLAN_DOC, key="C3c")
        command = re.search(r"`(aq message send [^`]+)`", footer).group(1)
        for project in (*importer.REPO_KIND, "renamed-project"):
            with self.subTest(project=project):
                # Intercept aq in the child shell to inspect argv without sending messages.
                result = subprocess.run(
                    ["sh", "-c", "aq() { printf '%s\\n' \"$@\"; }\n" + command],
                    env=dict(os.environ, AQ_PROJECT_ID=project),
                    capture_output=True, text=True, check=True,
                )
                self.assertEqual(result.stdout.splitlines(), [
                    "message", "send", "--to", "user:dashboard", "--project", project,
                    "--body", "Blocked on C3c (<task id>): <the decision needed>",
                ])


if __name__ == "__main__":
    unittest.main()
