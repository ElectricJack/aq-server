#!/usr/bin/env python3
"""Fast migration numbering and real Git merge regression checks."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

from migration_catalog import ROOT, state_format


class MigrationCatalogTests(unittest.TestCase):
    def test_rejects_duplicate_missing_and_noncanonical_numbers(self):
        for names in [[], ["0001_a.sql", "0001_b.sql"], ["0001_a.sql", "0003_c.sql"],
                      ["0000_a.sql", "0001_b.sql"], ["1_a.sql"], ["0001_.sql"]]:
            with self.subTest(names=names), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                directory = root / "crates/aq-server/migrations"
                directory.mkdir(parents=True)
                for name in names:
                    (directory / name).write_text("-- fixture\n")
                with self.assertRaises(ValueError):
                    state_format(root)

    def test_parallel_additions_merge_without_shared_registration_edits(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            shared = ["crates/aq-server/src/store.rs", "crates/aq-server/src/lib.rs", "Dockerfile"]
            for name in shared:
                destination = root / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / name, destination)
            shutil.copytree(ROOT / "crates/aq-server/migrations", root / "crates/aq-server/migrations")

            def git(*args):
                return subprocess.check_output(
                    ["git", "-c", "user.name=Migration fixture", "-c", "user.email=fixture@example.invalid",
                     "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args],
                    cwd=root, text=True, stderr=subprocess.STDOUT)

            git("init", "-b", "base")
            git("add", ".")
            git("commit", "-m", "base")
            latest = state_format(root)
            for branch, number in [("first", latest + 1), ("second", latest + 2)]:
                git("checkout", "-b", branch, "base")
                migration = root / f"crates/aq-server/migrations/{number:04}_{branch}.sql"
                migration.write_text(f"UPDATE meta SET value='{number}' WHERE key='state_format_version';\n")
                git("add", ".")
                git("commit", "-m", branch)
            git("checkout", "first")
            git("merge", "--no-edit", "second")
            self.assertEqual(git("diff", "base", "--", *shared), "")
            self.assertEqual(state_format(root), latest + 2)
            self.assertEqual(git("diff", "--name-only", "--diff-filter=U"), "")


if __name__ == "__main__":
    unittest.main()
