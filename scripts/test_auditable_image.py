"""Offline regressions for the auditable runtime-crate image evidence checks."""

import copy
import importlib.util
import json
import os
import re
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
ROOT = SCRIPTS.parent
VALIDATOR = SCRIPTS / "validate-auditable-image.py"
SCANNER = SCRIPTS / "scan-auditable-image.sh"
IMAGE_ID = "sha256:" + "a" * 64
SOURCE_SHA = "b" * 40
BINARY = "/home/nonroot/two-bot"
LOCK = """version = 4

[[package]]
name = "two-bot"
version = "0.1.0"

[[package]]
name = "tokio"
version = "1.40.0"

[[package]]
name = "libc"
version = "0.2.150"

[[package]]
name = "dev-only-crate"
version = "9.9.9"
"""

spec = importlib.util.spec_from_file_location("validate_auditable_image", VALIDATOR)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def artifact(name, version, **over):
    value = {
        "name": name, "version": version, "type": "rust-crate",
        "foundBy": "cargo-auditable-binary-cataloger",
        "locations": [{"path": BINARY}],
    }
    value.update(over)
    return value


def inventory():
    return {
        "source": {"type": "image", "metadata": {
            "imageID": IMAGE_ID, "userInput": IMAGE_ID,
            "os": "linux", "architecture": "amd64"}},
        "descriptor": {"name": "syft", "version": "1.56.0"},
        "artifacts": [artifact("two-bot", "0.1.0"), artifact("tokio", "1.40.0"),
                      artifact("libc", "0.2.150")],
    }


def report(matches=()):
    return {
        "descriptor": {"name": "grype", "version": "0.121.1"},
        "source": {"type": "image", "target": {"imageID": IMAGE_ID, "userInput": IMAGE_ID}},
        "matches": list(matches), "ignoredMatches": [],
    }


class ValidatorTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        (self.dir / "image-id.txt").write_text(IMAGE_ID + "\n")
        (self.dir / "source-sha.txt").write_text(SOURCE_SHA + "\n")
        (self.dir / "Cargo.lock").write_text(LOCK)
        self.write(inventory(), report())

    def write(self, inv=None, rep=None):
        if inv is not None:
            (self.dir / "runtime-crates.syft.json").write_text(json.dumps(inv))
        if rep is not None:
            (self.dir / "runtime-crates.grype.json").write_text(json.dumps(rep))

    def fails(self):
        with self.assertRaises((ValueError, KeyError, TypeError)):
            module.validate(self.dir)

    def test_valid_binds_provenance_and_keeps_dev_only_lockfile_extra(self):
        result = module.validate(self.dir)
        self.assertEqual(result["image_id"], IMAGE_ID)
        self.assertEqual(result["config_id"], IMAGE_ID)
        self.assertEqual(result["source_sha"], SOURCE_SHA)
        self.assertEqual(result["runtime_crates"], 3)
        self.assertEqual(result["grype_matches"], 0)
        self.assertEqual(result["suppressed_count"], 0)
        self.assertEqual(result["lockfile_only"],
                         [{"name": "dev-only-crate", "version": "9.9.9"}])
        self.assertEqual(set(result["sha256"]), {
            "Cargo.lock", "runtime-crates.syft.json", "runtime-crates.grype.json"})

    def assert_partition(self, result, runtime, locked):
        def tuples(rows):
            return {(row["name"], row["version"]) for row in rows}

        r = tuples(result["runtime_crate_tuples"])
        l = tuples(result["locked_crate_tuples"])
        o = tuples(result["lockfile_only"])
        self.assertEqual(r, runtime)
        self.assertEqual(l, locked)
        self.assertLessEqual(r, l)
        self.assertEqual(o, l - r)
        self.assertFalse(r & o)
        self.assertEqual(r | o, l)
        self.assertEqual(result["runtime_crates"], len(r))
        self.assertEqual(result["locked_crates"], len(l))
        self.assertEqual(result["runtime_completeness"], "not_proven")
        for key in ("runtime_crate_tuples", "locked_crate_tuples", "lockfile_only"):
            rows = [(row["name"], row["version"]) for row in result[key]]
            self.assertEqual(rows, sorted(set(rows)))

    def test_receipt_preserves_exact_separate_runtime_and_lockfile_views(self):
        runtime = {("two-bot", "0.1.0"), ("tokio", "1.40.0"), ("libc", "0.2.150")}
        self.assert_partition(module.validate(self.dir), runtime,
                              runtime | {("dev-only-crate", "9.9.9")})

    def test_partition_deduplicates_tuples_but_preserves_multiple_versions(self):
        inv = inventory()
        inv["artifacts"].extend([artifact("tokio", "1.40.0"), artifact("tokio", "1.39.0")])
        self.write(inv)
        (self.dir / "Cargo.lock").write_text(LOCK + '\n[[package]]\nname = "tokio"\n'
                                           'version = "1.39.0"\n\n[[package]]\n'
                                           'name = "tokio"\nversion = "1.40.0"\n')
        runtime = {("two-bot", "0.1.0"), ("tokio", "1.40.0"), ("tokio", "1.39.0"),
                   ("libc", "0.2.150")}
        self.assert_partition(module.validate(self.dir), runtime,
                              runtime | {("dev-only-crate", "9.9.9")})

    def test_empty_complement_is_valid_but_does_not_prove_runtime_completeness(self):
        inv = inventory()
        inv["artifacts"].append(artifact("dev-only-crate", "9.9.9"))
        self.write(inv)
        locked = {("two-bot", "0.1.0"), ("tokio", "1.40.0"), ("libc", "0.2.150"),
                  ("dev-only-crate", "9.9.9")}
        result = module.validate(self.dir)
        self.assert_partition(result, locked, locked)
        self.assertEqual(result["lockfile_only"], [])

    def test_containerd_manifest_id_binds_to_native_config_digest(self):
        inv, rep = inventory(), report()
        config_id = "sha256:" + "c" * 64
        inv["source"]["metadata"].update(
            imageID=config_id, repoDigests=["two-bot-next@" + IMAGE_ID])
        rep["source"]["target"]["imageID"] = config_id
        self.write(inv, rep)
        result = module.validate(self.dir)
        self.assertEqual(result["image_id"], IMAGE_ID)
        self.assertEqual(result["config_id"], config_id)
        for over in ({"repoDigests": []}, {"repoDigests": ["other@" + config_id]},
                     {"repoDigests": ["not-a-digest"]}, {"repoDigests": IMAGE_ID},
                     {"userInput": config_id}, {"imageID": "invalid"}):
            with self.subTest(over=over):
                wrong = copy.deepcopy(inv)
                wrong["source"]["metadata"].update(over)
                self.write(wrong, rep)
                self.fails()
        self.write(inv, rep)
        rep["source"]["target"]["imageID"] = IMAGE_ID
        self.write(rep=rep)
        self.fails()
