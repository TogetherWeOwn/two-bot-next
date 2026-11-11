"""Offline checks for the pinned commit-parser npm fixture.

The commit-parse guard needs release-please's parser. The fixture manifest and
lockfile pin it, and both workflow install steps must stage them to
RUNNER_TEMP and run `npm ci --ignore-scripts` rather than an ad-hoc
`npm install release-please@...`.
"""

import json
from pathlib import Path
import re
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "scripts/fixtures/commit-parser"
FIXTURE_REL = "scripts/fixtures/commit-parser"
PIN = "17.6.0"
WORKFLOWS = (
    ROOT / ".github/workflows/check.yml",
    ROOT / ".github/workflows/supply-chain.yml",
)


def load(name):
    return json.loads((FIXTURE / name).read_text(encoding="utf-8"))


def install_steps(path):
    """Return the run scripts of every step that installs the parser."""
    workflow = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    found = []
    for job in (workflow.get("jobs") or {}).values():
        for step in job.get("steps") or []:
            run = str(step.get("run", ""))
            if "release-please" in run or FIXTURE_REL in run:
                found.append(run)
    return found


class FixtureManifestTest(unittest.TestCase):
    def test_manifest_is_private_exact_pin_without_scripts(self):
        manifest = load("package.json")
        self.assertIs(manifest.get("private"), True)
        self.assertEqual(manifest.get("dependencies"), {"release-please": PIN})
        for key in ("devDependencies", "optionalDependencies", "peerDependencies", "scripts"):
            self.assertNotIn(key, manifest)

    def test_lock_matches_manifest_and_known_root(self):
        manifest = load("package.json")
        lock = load("package-lock.json")
        self.assertEqual(lock.get("lockfileVersion"), 3)
        self.assertEqual(lock["name"], manifest["name"])
        root = lock["packages"][""]
        self.assertEqual(root["name"], manifest["name"])
        self.assertEqual(root["dependencies"], manifest["dependencies"])
        self.assertEqual(lock["packages"]["node_modules/release-please"]["version"], PIN)

    def test_lock_entries_are_integrity_pinned_registry_tarballs(self):
        packages = load("package-lock.json")["packages"]
        for key, entry in packages.items():
            if key == "":
                continue
            with self.subTest(package=key):
                self.assertRegex(entry.get("resolved", ""), r"^https://registry\.npmjs\.org/")
                self.assertRegex(entry.get("integrity", ""), r"^sha(256|384|512)-")
                self.assertNotIn("hasInstallScript", entry)
                self.assertNotIn("link", entry)


class WorkflowInstallTest(unittest.TestCase):
    def test_pr_lint_sparse_checkout_includes_both_fixture_files(self):
        workflow = yaml.safe_load(WORKFLOWS[1].read_text())
        checkout = workflow["jobs"]["pr-lint"]["steps"][0]
        self.assertTrue(checkout["uses"].startswith("actions/checkout@"))
        options = checkout["with"]
        self.assertNotIn("path", options)
        self.assertIs(options["sparse-checkout-cone-mode"], True)
        paths = options["sparse-checkout"].splitlines()
        for name in ("package.json", "package-lock.json"):
            file = Path(FIXTURE_REL, name)
            self.assertTrue(any(Path(path) in file.parents for path in paths),
                            f"sparse checkout excludes {file}")
        self.assertIn(".github", paths)

    def test_each_workflow_has_one_conforming_install_step(self):
        for path in WORKFLOWS:
            with self.subTest(workflow=path.name):
                steps = install_steps(path)
                self.assertTrue(steps, "no parser install step found")
                joined = "\n".join(steps)
                self.assertIsNone(re.search(r"npm\s+install\b", joined), "uses npm install")
                self.assertNotIn("release-please@", joined, "installs a package spec")
                self.assertIn("RUNNER_TEMP", joined)
                for name in ("package.json", "package-lock.json"):
                    self.assertRegex(
                        joined,
                        rf"{re.escape(FIXTURE_REL)}/{re.escape(name)}",
                        f"{name} is not staged",
                    )
                self.assertRegex(joined, r"\bcp\b")
                ci = re.findall(r"npm\s+ci\b[^\n]*", joined)
                self.assertTrue(ci, "no npm ci")
                for command in ci:
                    self.assertIn("--ignore-scripts", command)


if __name__ == "__main__":
    unittest.main()
