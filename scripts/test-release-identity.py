#!/usr/bin/env python3
"""Keep release, promotion and host verification on the same signing identity."""
from pathlib import Path
import re
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW_URL = "https://github.com/ElectricJack/aq-server/.github/workflows/release.yml"


def production_identities():
    identities = {}
    for name in ("release.yml", "promote.yml"):
        workflow = (ROOT / ".github/workflows" / name).read_text(encoding="utf-8")
        values = re.findall(r"^  COSIGN_IDENTITY_REGEXP: '([^'\n]+)'$", workflow, re.M)
        assert len(values) == 1, f"expected one identity regexp in {name}"
        identities[name] = values[0]

    operations = (ROOT / "docs/operations.md").read_text(encoding="utf-8")
    updater_section = operations.split("### updater.toml\n", 1)[1]
    updater_toml = updater_section.split("```toml\n", 1)[1].split("```", 1)[0]
    identities["updater.toml example"] = tomllib.loads(updater_toml)["verifier"]["identity_regexp"]
    # Cover the trust block and the manual worker verification command too.
    documented = re.findall(r"--certificate-identity-regexp '([^'\n]+)'", operations)
    assert documented, "expected documented cosign identity arguments"
    identities.update({f"cosign example {i}": value for i, value in enumerate(documented)})
    return identities


class ReleaseIdentityTests(unittest.TestCase):
    def test_production_identities_agree(self):
        identities = production_identities()
        for source, identity in identities.items():
            with self.subTest(source=source):
                self.assertEqual(identity, identities["release.yml"])

    def test_only_current_repository_release_tags_are_trusted(self):
        valid = WORKFLOW_URL + "@refs/tags/v1.2.3"
        rejected = (
            valid.replace("/aq-server/", "/agent-q/"),
            WORKFLOW_URL + "@refs/heads/main",
            "prefix" + valid,
            valid + "/suffix",
        )
        for source, identity in production_identities().items():
            with self.subTest(source=source):
                self.assertTrue(identity.startswith("^"))
                self.assertTrue(identity.endswith("$"))
                pattern = re.compile(identity)
                self.assertIsNotNone(pattern.search(valid))
                for value in rejected:
                    with self.subTest(value=value):
                        self.assertIsNone(pattern.search(value))


if __name__ == "__main__":
    unittest.main()
