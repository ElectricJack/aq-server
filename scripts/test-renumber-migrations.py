#!/usr/bin/env python3
"""Renumbering a branch's migrations onto a moved origin/main, in a temporary Git repository."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
SCRIPT = SCRIPTS / "renumber-migrations.py"
sys.path.insert(0, str(SCRIPTS))
from migration_catalog import state_format  # noqa: E402

MIGRATIONS = "crates/aq-server/migrations"
SOURCE = "crates/aq-server/src/gate.rs"


def migration(number, extra=""):
    return f"{extra}UPDATE meta SET value='{number}' WHERE key='state_format_version';\n"


class RenumberTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.git("init", "-b", "main")
        for number, name in [(1, "one"), (2, "two")]:
            self.write(f"{MIGRATIONS}/{number:04}_{name}.sql", migration(number))
        self.write(SOURCE, "fn base() {\n    if supported >= 2 { two(); }\n}\n// tail\n")
        self.commit("base")

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-c", "user.name=Renumber fixture", "-c", "user.email=fixture@example.invalid",
             "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", *args],
            cwd=self.root, text=True, stderr=subprocess.STDOUT)

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-m", message)

    def move_main(self, *numbers):
        """Fake origin/main: main gains migrations `numbers`, and its own gate in the source tail."""
        self.git("checkout", "-b", "upstream", "main")
        for number in numbers:
            self.write(f"{MIGRATIONS}/{number:04}_upstream{number}.sql", migration(number))
        text = (self.root / SOURCE).read_text()
        (self.root / SOURCE).write_text(text + "".join(f"if supported >= {n} {{ up{n}(); }}\n" for n in numbers))
        self.commit("main moves")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.git("checkout", "main")

    def branch_adds(self, *numbers):
        self.git("checkout", "-b", "feature", "main")
        for number in numbers:
            self.write(f"{MIGRATIONS}/{number:04}_feature{number}.sql",
                       migration(number, f"-- migration {number} adds a column; see PRIORS_{number}.\n"))
        # The branch's edit sits at the top; main's edit is in the tail, so the merge is clean.
        text = (self.root / SOURCE).read_text()
        gates = "".join(f"// FEATURE_FORMAT {n}\nif supported >= {n} {{ feature{n}(); }}\n"
                        f"const PRIORS_{n}: &str = \"{n:04}_feature{n}.sql\";\n" for n in numbers)
        self.write(SOURCE, gates + text)
        self.commit("feature")

    def run_script(self, *args):
        return subprocess.run([sys.executable, str(SCRIPT), "--root", str(self.root), *args],
                              text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    def names(self):
        return sorted(path.name for path in (self.root / MIGRATIONS).glob("*.sql"))

    def test_no_new_migrations(self):
        self.move_main(3)
        self.git("checkout", "-b", "feature", "main")
        self.write("README", "nothing\n")
        self.commit("no migration")
        before = self.names()
        for args in [["--check"], []]:
            result = self.run_script(*args)
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertIn("no renumbering needed", result.stdout)
        self.assertEqual(self.names(), before)
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_new_migration_already_following_main_is_left_alone(self):
        self.move_main(3)
        self.branch_adds(4)
        self.git("merge", "--no-edit", "upstream")
        self.assertEqual(self.run_script("--check").returncode, 0)
        self.assertEqual(self.run_script().returncode, 0)
        self.assertEqual(self.git("status", "--porcelain"), "")
        self.assertEqual(state_format(self.root), 4)

    def test_one_colliding_migration(self):
        self.move_main(3)
        self.branch_adds(3)
        self.git("merge", "--no-edit", "upstream")  # file names differ, so Git merges cleanly
        with self.assertRaises(ValueError):
            state_format(self.root)
        before = self.git("status", "--porcelain")

        check = self.run_script("--check")
        self.assertEqual(check.returncode, 1, check.stdout)
        self.assertIn("0003_feature3.sql -> 0004_feature3.sql", check.stdout)
        self.assertEqual(self.git("status", "--porcelain"), before)
        self.assertTrue((self.root / MIGRATIONS / "0003_feature3.sql").exists())

        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(self.names(), ["0001_one.sql", "0002_two.sql", "0003_upstream3.sql",
                                        "0004_feature3.sql"])
        self.assertEqual(state_format(self.root), 4)
        self.assertEqual((self.root / MIGRATIONS / "0004_feature3.sql").read_text(),
                         migration(4, "-- migration 4 adds a column; see PRIORS_4.\n"))
        source = (self.root / SOURCE).read_text()
        self.assertIn("// FEATURE_FORMAT 4\nif supported >= 4 { feature3(); }\n"
                      'const PRIORS_4: &str = "0004_feature3.sql";\n', source)
        # Main's own gate for its migration 3 is not the branch's line and stays.
        self.assertIn("if supported >= 3 { up3(); }\n", source)
        self.assertEqual(self.migration_files_unchanged_from_main(), "")
        # Idempotent: nothing more to do.
        self.assertEqual(self.run_script("--check").returncode, 0)

    def migration_files_unchanged_from_main(self):
        return self.git("diff", "origin/main", "--", f"{MIGRATIONS}/0003_upstream3.sql")

    def test_two_consecutive_colliding_migrations_before_merging_main(self):
        self.move_main(3, 4)
        self.branch_adds(3, 4)
        check = self.run_script("--check")
        self.assertEqual(check.returncode, 1, check.stdout)

        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("merge origin/main", result.stdout)  # main's 0003 and 0004 are not in the tree yet
        self.assertEqual(self.names(), ["0001_one.sql", "0002_two.sql", "0005_feature3.sql",
                                        "0006_feature4.sql"])
        self.assertEqual((self.root / MIGRATIONS / "0005_feature3.sql").read_text(),
                         migration(5, "-- migration 5 adds a column; see PRIORS_5.\n"))
        self.assertEqual((self.root / MIGRATIONS / "0006_feature4.sql").read_text(),
                         migration(6, "-- migration 6 adds a column; see PRIORS_6.\n"))
        source = (self.root / SOURCE).read_text()
        self.assertIn("if supported >= 5 { feature3(); }", source)
        self.assertIn("if supported >= 6 { feature4(); }", source)
        self.assertIn('const PRIORS_6: &str = "0006_feature4.sql";', source)

        # The renumbered branch merges with main without editing a shared file, and the catalog passes.
        self.commit("renumber")
        self.git("merge", "--no-edit", "upstream")
        self.assertEqual(state_format(self.root), 6)
        self.assertEqual(self.run_script("--check").returncode, 0)
        merged = (self.root / SOURCE).read_text()
        self.assertIn("if supported >= 3 { up3(); }\nif supported >= 4 { up4(); }\n", merged)

    def test_consecutive_numbers_shift_up_by_one_without_chaining(self):
        # Branch 0003/0004 over a main at 0003: 0003->0004 and 0004->0005, each rewritten once.
        self.move_main(3)
        self.branch_adds(3, 4)
        self.git("merge", "--no-edit", "upstream")
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(state_format(self.root), 5)
        self.assertEqual((self.root / MIGRATIONS / "0005_feature4.sql").read_text(),
                         migration(5, "-- migration 5 adds a column; see PRIORS_5.\n"))
        self.assertEqual((self.root / MIGRATIONS / "0004_feature3.sql").read_text(),
                         migration(4, "-- migration 4 adds a column; see PRIORS_4.\n"))


if __name__ == "__main__":
    unittest.main()
