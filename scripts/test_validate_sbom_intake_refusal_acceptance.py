"""Protected acceptance: validate-sbom refuses oversized, ambiguous and missing intake.

``validate-sbom.py`` reads every intake unbounded with bare ``json.loads`` /
``read_text`` calls and no CLI error handling, so hostile or absent input
either parses last-wins or escapes as a traceback:

* over-cap files are fully buffered and parsed (no byte cap);
* duplicate JSON keys last-wins past the lockfile comparison;
* ``NaN``/``Infinity``/out-of-range floats parse and flow into the comparison;
* deeply nested ``components`` recurse unbounded (``RecursionError``);
* missing files escape as ``FileNotFoundError`` tracebacks;
* missing CLI args escape as ``IndexError`` tracebacks.

The fix must refuse every mode with fixed vocabulary, no traceback, no echo
of intake bytes, and exit 1 (refusals) or 2 (argparse usage). Exact contract:

* ``OVER_CAP`` when any intake file exceeds the byte cap;
* ``DUP_KEYS`` when any BOM carries duplicate JSON object keys;
* ``BAD_CONST`` when any BOM carries a non-finite JSON number;
* ``TOO_DEEP`` when component nesting exceeds the bound;
* ``MISSING_FILE`` when any intake file is missing or unreadable;
* argparse usage (exit 2) when CLI args are missing.

Over-cap fixtures are 8 MiB (JSON) / 1 MiB (text): the builder's caps sit at
or below those ceilings while real intake (~17 KiB BOM, ~66 KiB lockfile)
keeps passing. The nesting probe is under 64 KiB so it isolates the depth
bound from the byte cap. Passing controls pin valid intake and the existing
semantic messages, which must stay byte-identical.

RED expectations on the current code: over-cap/duplicate/constant fixtures
are buffered and parsed (accepted, or refused with the wrong message);
missing files/args leak ``FileNotFoundError``/``IndexError`` tracebacks;
deep nesting leaks a ``RecursionError`` traceback.

Offline only: fixtures are temp files; no network, no credentials.
Builder must not edit this file; build on the pushed head.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_validate_sbom_intake_refusal_acceptance.py' -v
"""

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("validate-sbom.py")
spec = importlib.util.spec_from_file_location("validate_sbom", SCRIPT)
validator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validator)


OVER_CAP = "SBOM input exceeds the byte cap"
DUP_KEYS = "SBOM JSON carries duplicate object keys"
BAD_CONST = "SBOM JSON carries a non-finite number"
TOO_DEEP = "SBOM components exceed the nesting bound"
MISSING_FILE = "SBOM input file is missing or unreadable"
CANARY = "VALIDATE-SBOM-CANARY-NEVER-ECHO-7KQ1"

BIG_JSON = 8 << 20
BIG_TEXT = 1 << 20
NESTING_LEVELS = 1500

SHA = "a" * 40
IMAGE_ID = "sha256:" + "b" * 64
PACKAGES = (("alpha", "1.0.0"), ("beta", "2.0.0"))


def scratch_dir():
    return (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
            or os.environ.get("PAPERCLIP_SCRATCH_DIR")
            or os.environ.get("RUNNER_TEMP"))


class ValidateSbomIntakeRefusalAcceptance(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(dir=scratch_dir())
        self.addCleanup(tmp.cleanup)
        self.directory = Path(tmp.name)
        self.lockfile = self.directory / "Cargo.lock"
        self.lockfile.write_text("".join(
            '[[package]]\nname = "%s"\nversion = "%s"\n\n' % pkg for pkg in PACKAGES))
        self.write_valid_boms()
        (self.directory / "source-sha.txt").write_text(SHA + "\n")
        (self.directory / "image-id.txt").write_text(IMAGE_ID + "\n")

    def components(self):
        return [{"name": name, "version": version} for name, version in PACKAGES]

    def write_bom(self, name, inventory):
        (self.directory / f"{name}.cdx.json").write_text(json.dumps(
            {"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1,
             "components": inventory}))

    def write_valid_boms(self):
        self.write_bom("rust-workspace", self.components())
        self.write_bom("container-image", [{"name": "ca-certificates", "version": "fixture",
                                            "purl": "pkg:deb/debian/ca-certificates@fixture"}])

    def validate(self):
        return validator.validate(self.lockfile, self.directory)

    def run_cli(self, *args):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *[str(arg) for arg in args]],
            capture_output=True, text=True, timeout=60,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})

    def assert_cli_refusal(self, result, message, label):
        combined = result.stdout + result.stderr
        self.assertEqual(result.returncode, 1, f"over-accepted input in case {label}")
        self.assertNotIn("Traceback", combined, f"traceback leaked in case {label}")
        self.assertIn(message, combined, f"fixed refusal missing in case {label}")
        self.assertNotIn(CANARY, combined, f"intake echoed in case {label}")

    def padded_bom_text(self, name):
        inventory = (self.components() if name == "rust-workspace" else
                     [{"name": "ca-certificates", "version": "fixture",
                       "purl": "pkg:deb/debian/ca-certificates@fixture"}])
        text = json.dumps({"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1,
                           "components": inventory,
                           "padding": CANARY + "x" * BIG_JSON})
        self.assertGreaterEqual(len(text), BIG_JSON)
        return text

    # --- over-cap intake ---------------------------------------------------

    def test_over_cap_rust_bom_refuses(self):
        (self.directory / "rust-workspace.cdx.json").write_text(
            self.padded_bom_text("rust-workspace"))
        with self.assertRaises(ValueError) as caught:
            self.validate()
        self.assertEqual(str(caught.exception), OVER_CAP)

    def test_over_cap_image_bom_refuses(self):
        (self.directory / "container-image.cdx.json").write_text(
            self.padded_bom_text("container-image"))
        with self.assertRaises(ValueError) as caught:
            self.validate()
        self.assertEqual(str(caught.exception), OVER_CAP)

    def test_over_cap_lockfile_refuses(self):
        with open(self.lockfile, "a") as handle:
            handle.write("# " + CANARY + "x" * BIG_JSON + "\n")
        self.assertGreaterEqual(self.lockfile.stat().st_size, BIG_JSON)
        with self.assertRaises(ValueError) as caught:
            self.validate()
        self.assertEqual(str(caught.exception), OVER_CAP)

    def test_over_cap_provenance_refuses(self):
        for name in ("source-sha.txt", "image-id.txt"):
            with self.subTest(name=name):
                (self.directory / name).write_text(CANARY + "x" * BIG_TEXT)
                with self.assertRaises(ValueError) as caught:
                    self.validate()
                self.assertEqual(str(caught.exception), OVER_CAP)
                self.write_valid_boms()
                (self.directory / "source-sha.txt").write_text(SHA + "\n")
                (self.directory / "image-id.txt").write_text(IMAGE_ID + "\n")

    def test_cli_over_cap_refuses_without_traceback(self):
        (self.directory / "rust-workspace.cdx.json").write_text(
            self.padded_bom_text("rust-workspace"))
        self.assert_cli_refusal(self.run_cli(self.lockfile, self.directory),
                                OVER_CAP, "rust-bom")

    # --- duplicate JSON keys (last-wins today) -------------------------------

    def test_duplicate_keys_refuse(self):
        text = ('{"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1, '
                '"note": "%s", "note": "%s", "components": '
                '[{"name": "alpha", "name": "alpha", "version": "1.0.0"}, '
                '{"name": "beta", "version": "2.0.0"}]}') % (CANARY, CANARY)
        (self.directory / "rust-workspace.cdx.json").write_text(text)
        with self.assertRaises(ValueError) as caught:
            self.validate()
        self.assertEqual(str(caught.exception), DUP_KEYS)

    def test_nested_duplicate_keys_refuse(self):
        text = ('{"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1, '
                '"components": [{"name": "wrap", "components": '
                '[{"name": "alpha", "version": "1.0.0", "version": "1.0.0"}, '
                '{"name": "beta", "version": "2.0.0"}]}]}')
        (self.directory / "rust-workspace.cdx.json").write_text(text)
        with self.assertRaises(ValueError) as caught:
            self.validate()
        self.assertEqual(str(caught.exception), DUP_KEYS)

    def test_cli_duplicate_keys_refuse_without_traceback(self):
        text = ('{"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1, '
                '"note": "%s", "note": "%s", "components": '
                '[{"name": "alpha", "version": "1.0.0"}, '
                '{"name": "beta", "version": "2.0.0"}]}') % (CANARY, CANARY)
        (self.directory / "rust-workspace.cdx.json").write_text(text)
        self.assert_cli_refusal(self.run_cli(self.lockfile, self.directory),
                                DUP_KEYS, "duplicate-keys")

    # --- out-of-range constants (parsed today) ---------------------------------

    def test_out_of_range_constant_refuses(self):
        cases = (
            ("1e1000",
             '{"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1e1000, '
             '"components": [{"name": "alpha", "version": "1.0.0"}, '
             '{"name": "beta", "version": "2.0.0"}]}'),
            ("NaN",
             '{"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1, '
             '"components": [{"name": "alpha", "version": NaN}, '
             '{"name": "beta", "version": "2.0.0"}]}'),
            ("Infinity",
             '{"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1, '
             '"components": [{"name": "alpha", "version": "1.0.0"}, '
             '{"name": "beta", "version": Infinity}]}'),
        )
        for constant, text in cases:
            with self.subTest(constant=constant):
                (self.directory / "rust-workspace.cdx.json").write_text(text)
                with self.assertRaises(ValueError) as caught:
                    self.validate()
                self.assertEqual(str(caught.exception), BAD_CONST)

    def test_cli_out_of_range_constant_refuses_without_traceback(self):
        text = ('{"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1e1000, '
                '"note": "%s", "components": [{"name": "alpha", "version": "1.0.0"}, '
                '{"name": "beta", "version": "2.0.0"}]}') % CANARY
        (self.directory / "rust-workspace.cdx.json").write_text(text)
        self.assert_cli_refusal(self.run_cli(self.lockfile, self.directory),
                                BAD_CONST, "non-finite")

    # --- unbounded component nesting (RecursionError today) --------------------

    def nested_text(self):
        leaf = ('{"name": "alpha", "version": "1.0.0"}, '
                '{"name": "beta", "version": "2.0.0"}')
        text = ('{"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1, '
                '"components": [' + '{"name": "w", "components": [' * NESTING_LEVELS
                + leaf + ']}' * NESTING_LEVELS + ']}')
        # The probe must stay far under the byte cap so it isolates the depth
        # bound: a cap refusal here would be the wrong contract.
        self.assertLess(len(text), 64 << 10)
        return text

    def test_cli_deep_nesting_refuses_without_traceback(self):
        (self.directory / "rust-workspace.cdx.json").write_text(self.nested_text())
        self.assert_cli_refusal(self.run_cli(self.lockfile, self.directory),
                                TOO_DEEP, "nesting")

    # --- missing files/args (tracebacks today) ---------------------------------

    def test_cli_missing_file_refuses_without_traceback(self):
        victims = ["rust-workspace.cdx.json", "container-image.cdx.json",
                   "source-sha.txt", "image-id.txt", "Cargo.lock"]
        for name in victims:
            with self.subTest(name=name):
                path = self.directory / name
                content = path.read_bytes()
                path.unlink()
                try:
                    result = self.run_cli(self.lockfile, self.directory)
                finally:
                    path.write_bytes(content)
                self.assert_cli_refusal(result, MISSING_FILE, name)

    def test_cli_missing_directory_refuses_without_traceback(self):
        missing = self.directory / "no-such-dir"
        result = self.run_cli(self.lockfile, missing)
        self.assert_cli_refusal(result, MISSING_FILE, "directory")

    def test_cli_missing_args_is_usage_error_without_traceback(self):
        for args in ((), (self.lockfile,)):
            with self.subTest(args=len(args)):
                result = self.run_cli(*args)
                combined = result.stdout + result.stderr
                self.assertEqual(result.returncode, 2, f"accepted {len(args)} args")
                self.assertNotIn("Traceback", combined, "traceback leaked")
                self.assertIn("usage", combined.lower(), "usage line missing")

    # --- passing controls ------------------------------------------------------

    def test_control_valid_small_intake_passes(self):
        self.assertIsNone(self.validate())

    def test_cli_control_valid_small_intake_passes(self):
        result = self.run_cli(self.lockfile, self.directory)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PASS CycloneDX inventories", result.stdout)
        self.assertEqual(result.stderr, "")

    def test_control_semantic_messages_unchanged(self):
        self.write_bom("rust-workspace", [])
        with self.assertRaisesRegex(ValueError, "Empty SBOM"):
            self.validate()
        # validate() reads the rust BOM first: restore it before probing the
        # image BOM, or the empty-rust refusal masks the image refusal.
        self.write_valid_boms()
        (self.directory / "container-image.cdx.json").write_text(
            '{"components": [{"name": "fake"}]}')
        with self.assertRaisesRegex(ValueError, "Not a CycloneDX"):
            self.validate()
        self.write_valid_boms()
        self.write_bom("rust-workspace", self.components()[1:])
        with self.assertRaisesRegex(ValueError, "omitted locked packages"):
            self.validate()


if __name__ == "__main__":
    unittest.main()
