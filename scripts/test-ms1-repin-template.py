#!/usr/bin/env python3
"""ms1-repin-template.py on a synthetic config: template -> environment -> qualification, chained."""
import importlib.util
import json
import stat
import tempfile
import tomllib
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("ms1-repin-template.py")
spec = importlib.util.spec_from_file_location("ms1_repin_template", SCRIPT)
repin = importlib.util.module_from_spec(spec)
spec.loader.exec_module(repin)

OLD_T, NEW_T = "a" * 64, "b" * 64
HELPER = repin.HELPER
TEMPLATE, ENVIRONMENT = "aq.sandbox-template", "aq.sandbox-environment"
COMPOSITE, INVENTORY = repin.COMPOSITE, repin.INVENTORY
UNTOUCHED_BYTES = '{ "kind" : "composite",  "note": "keep these exact bytes" }\n'
OPAQUE_BYTES = "not json at all"


def ref(artifact):
    return {"id": artifact["id"], "version": artifact["version"], "digest": artifact["digest"]}


def definition(ident, version, value=None, body=None):
    body = repin.canonical(value) if body is None else body
    return {"artifact": {"id": ident, "version": version, "digest": repin.sha(body)}, "body": body, "kind": "definition"}


class Fixture:
    def __init__(self, root, helper_bytes=b"old helper", helper_path="/old/aq-session-host"):
        self.root = Path(root)
        helper_path = str(helper_path)
        old_helper = {"artifact": {"digest": repin.sha(helper_bytes), "id": HELPER, "version": "1"},
                      "bytes": len(helper_bytes), "path": helper_path}
        template = definition(TEMPLATE, "2", {"image": f"registry/sandbox@sha256:{OLD_T}", "helper": old_helper})
        environment = definition(ENVIRONMENT, "5", {"template": ref(template["artifact"]), "limits": {"cpu": 2},
                                                    "helper": {"artifact": old_helper["artifact"], "path": helper_path}})
        qualification = definition(INVENTORY, "3", {"environment": ref(environment["artifact"])})
        composite = definition(COMPOSITE, "7", body=UNTOUCHED_BYTES)
        opaque = definition("aq.opaque", "9", body=OPAQUE_BYTES)
        self.bodies = [template, environment, qualification, composite, opaque]
        self.profiled = {"revision": 11, "definition_bodies": self.bodies,
                         "qualified_runtimes": [{"runtime": "r", "qualification": ref(qualification["artifact"])}]}
        self.intelligence = {"revision": 4, "qualified_harnesses": [{"harness": "h", "environment": ref(environment["artifact"])}]}
        self.native = {"template": ref(template["artifact"]), "helper": old_helper, "other": [ref(qualification["artifact"])]}
        text = "\n".join([
            'title = "kept"', "",
            "[intelligence]", f"revision = {repin.toml_value(self.intelligence['revision'])}",
            f"qualified_harnesses = {repin.toml_value(self.intelligence['qualified_harnesses'])}", "",
            "[profiled_execution]", f"revision = {repin.toml_value(self.profiled['revision'])}",
            f"definition_bodies = {repin.toml_value(self.bodies)}",
            f"qualified_runtimes = {repin.toml_value(self.profiled['qualified_runtimes'])}", "",
            "[other]", 'untouched = "yes"', ""])
        self.config = self.root / "config.toml"
        self.config.write_text(text)
        self.native_path = self.root / "native.json"
        self.native_path.write_text(json.dumps(self.native))
        self.helper = self.root / "new-helper"
        self.helper.write_bytes(b"brand new helper bytes")
        self.out = self.root / "out"

    def run(self, *extra):
        return repin.main([str(self.config), str(self.native_path), str(self.out), *extra])

    def output(self, name):
        return (self.out / name).read_text()


class RepinTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.f = Fixture(directory.name)

    def result(self):
        config = tomllib.loads(self.f.output("config.toml"))
        bodies = {d["artifact"]["id"]: d for d in config["profiled_execution"]["definition_bodies"]}
        return config, bodies

    def check_chain(self, config, bodies, version):
        old = {d["artifact"]["id"]: d for d in self.f.bodies}
        for d in bodies.values():  # every digest is the sha256 of its body
            self.assertEqual(d["artifact"]["digest"], repin.sha(d["body"]))
        for ident in (TEMPLATE, ENVIRONMENT, INVENTORY):
            self.assertEqual(bodies[ident]["artifact"]["version"], version)
            self.assertNotEqual(bodies[ident]["artifact"]["digest"], old[ident]["artifact"]["digest"])
        environment = json.loads(bodies[ENVIRONMENT]["body"])
        self.assertEqual(environment["template"], ref(bodies[TEMPLATE]["artifact"]))
        self.assertEqual(json.loads(bodies[INVENTORY]["body"])["environment"], ref(bodies[ENVIRONMENT]["artifact"]))
        profiled, intelligence = config["profiled_execution"], config["intelligence"]
        self.assertEqual(profiled["qualified_runtimes"][0]["qualification"], ref(bodies[INVENTORY]["artifact"]))
        self.assertEqual(intelligence["qualified_harnesses"][0]["environment"], ref(bodies[ENVIRONMENT]["artifact"]))
        self.assertEqual((profiled["revision"], intelligence["revision"]), (12, 5))
        for ident in (COMPOSITE, "aq.opaque"):  # untouched bodies keep their exact bytes and artifacts
            self.assertEqual(bodies[ident], old[ident])
        self.assertEqual(config["title"], "kept")
        self.assertEqual(config["other"], {"untouched": "yes"})

    def test_template_and_helper_change_settles_the_chain(self):
        self.assertEqual(self.f.run("--template-digest", NEW_T, "--old-template-digest", OLD_T,
                                    "--helper-path", str(self.f.helper)), 0)
        config, bodies = self.result()
        self.check_chain(config, bodies, "10")  # max numeric version is 9
        template = json.loads(bodies[TEMPLATE]["body"])
        self.assertEqual(template["image"], f"registry/sandbox@sha256:{NEW_T}")
        data = self.f.helper.read_bytes()
        pin = {"artifact": {"digest": repin.sha(data), "id": HELPER, "version": "1"}, "bytes": len(data), "path": str(self.f.helper)}
        self.assertEqual(template["helper"], pin)
        # A pin without `bytes` keeps that shape.
        self.assertEqual(json.loads(bodies[ENVIRONMENT]["body"])["helper"], {"artifact": pin["artifact"], "path": pin["path"]})
        self.assertNotIn(OLD_T, self.f.output("config.toml").replace(OPAQUE_BYTES, ""))

    def test_parent_uses_final_digest_when_environment_precedes_template(self):
        bodies = self.f.bodies
        by_id = {b["artifact"]["id"]: b for b in bodies}
        first = [by_id[ENVIRONMENT], by_id[TEMPLATE], by_id[INVENTORY]]
        reordered = first + [b for b in bodies if b not in first]
        config = self.f.config.read_text()
        self.f.config.write_text(config.replace(repin.toml_value(bodies), repin.toml_value(reordered)))
        self.f.run("--old-template-digest", OLD_T, "--template-digest", NEW_T,
                   "--helper-path", str(self.f.helper))
        self.check_chain(*self.result(), "10")

    def test_node_files_follow(self):
        self.f.run("--template-digest", NEW_T, "--old-template-digest", OLD_T, "--helper-path", str(self.f.helper))
        config, bodies = self.result()
        native = json.loads(self.f.output("native.json"))
        self.assertEqual(native["template"], ref(bodies[TEMPLATE]["artifact"]))
        self.assertEqual(native["other"], [ref(bodies[INVENTORY]["artifact"])])
        self.assertEqual(native["helper"]["bytes"], len(self.f.helper.read_bytes()))
        self.assertEqual(self.f.output("composite.json"), UNTOUCHED_BYTES)
        self.assertEqual(self.f.output("inventory.json"), bodies[INVENTORY]["body"])
        self.assertFalse((self.f.out / "codex-composite.json").exists())
        for name in ("config.toml", "native.json", "composite.json", "inventory.json"):
            self.assertEqual(stat.S_IMODE((self.f.out / name).stat().st_mode), 0o600)

    def test_helper_only_leaves_the_template_alone(self):
        self.f.run("--helper-path", str(self.f.helper))
        config, bodies = self.result()
        self.assertEqual(json.loads(bodies[TEMPLATE]["body"])["image"], f"registry/sandbox@sha256:{OLD_T}")
        self.check_chain(config, bodies, "10")

    def test_template_only_keeps_the_helper_pin(self):
        self.f.run("--template-digest", NEW_T, "--old-template-digest", OLD_T)
        config, bodies = self.result()
        self.assertEqual(json.loads(bodies[TEMPLATE]["body"])["helper"]["path"], "/old/aq-session-host")
        self.check_chain(config, bodies, "10")

    def test_refuses_without_a_change(self):
        with self.assertRaises(SystemExit):
            self.f.run()
        with self.assertRaises(SystemExit):  # the digest is not in any body
            self.f.run("--template-digest", NEW_T, "--old-template-digest", "c" * 64)
        with self.assertRaises(SystemExit):
            self.f.run("--template-digest", OLD_T, "--old-template-digest", OLD_T)
        with self.assertRaises(SystemExit):  # the old digest must come with the new
            self.f.run("--template-digest", NEW_T)
        with self.assertRaises(SystemExit):
            self.f.run("--template-digest", "XYZ", "--old-template-digest", OLD_T)
        self.assertFalse(self.f.out.exists())

    def test_pinning_the_helper_already_pinned_changes_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            helper = Path(directory) / "helper"
            helper.write_bytes(b"old helper")
            fixture = Fixture(directory, helper_path=helper)
            fixture.out = Path(directory) / "out2"
            with self.assertRaises(SystemExit):
                fixture.run("--helper-path", str(helper))
            self.assertFalse(fixture.out.exists())


if __name__ == "__main__":
    unittest.main()
