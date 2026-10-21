#!/usr/bin/env python3
"""Acceptance tests: malformed image inspection fails clean, not traceback.

Protected acceptance file: the implementation change must satisfy these
tests without editing them. Test defects are corrected here, not by
weakening assertions in the build.

Contract under test (``scripts/runtime-image-evidence.py`` owns the
``docker image inspect`` parse at :287-301):

* Empty inspect arrays, non-JSON stdout, non-list JSON, non-object
  elements and objects missing ``Id``/``Config``/``Architecture``/``Os``
  become the file's own ``collection_error`` path (the
  ``inspect_filesystem`` precedent at :314-317): ``collect()`` returns a
  report with ``complete`` false, a fixed-vocabulary ``collection_error``
  string and no ``filesystem`` key; the JSON file is still written; only
  the ``image inspect`` Docker call has run (no create/export/rm).
* ``main()`` surfaces the same text via ``SystemExit`` (clean nonzero
  exit): no ``IndexError``/``KeyError``/``ValueError``/``TypeError``
  escapes, no traceback is printed, and the diagnostic stays bounded
  (at most 300 characters) with fixed vocabulary naming inspection and
  a failure token (unparseable/failed/invalid/malformed). Docker stdout
  bodies are never echoed.
* Valid inspection stays byte-identical: a well-formed report is
  complete with the same schema, provenance and package rows.
* Boundaries are preserved: an identity mismatch still raises
  ``ValueError`` before any container is created, and invalid provenance
  still raises before any Docker call (existing tests pin both).

Docker is fully stubbed: no daemon, no containers, no network.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_runtime_image_inspect_acceptance.py' -v
"""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

_spec = importlib.util.spec_from_file_location(
    "test_runtime_image_evidence_base",
    Path(__file__).with_name("test-runtime-image-evidence.py"),
)
_base = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_base)

evidence = _base.evidence
ImageFixture = _base.ImageFixture
FakeDocker = _base.FakeDocker
IMAGE_ID = _base.IMAGE_ID
SOURCE_SHA = _base.SOURCE_SHA
METADATA = _base.METADATA

# Assembled from fragments so no credential-shaped literal is stored.
BODY_SENTINEL = "InspectBody" + "NeverEcho" + "7Q"
MAX_DIAGNOSTIC_LINE = 300


def scratch_dir():
    base = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")
    return tempfile.TemporaryDirectory(dir=base)


def write_provenance(directory):
    (directory / "image-id.txt").write_text(IMAGE_ID + "\n")
    (directory / "source-sha.txt").write_text(SOURCE_SHA + "\n")


class InspectStub:
    """Stubbed docker returning fixed inspect stdout; records every call."""

    def __init__(self, stdout, returncode=0, stderr=b""):
        self.stdout = stdout
        self.returncode = returncode
        self.stderr = stderr
        self.calls = []

    def __call__(self, *args, timeout=30):
        self.calls.append(args)
        if args[:2] == ("image", "inspect"):
            return subprocess.CompletedProcess(args, self.returncode, self.stdout, self.stderr)
        raise AssertionError(f"unexpected docker call after failed inspection: {args!r}")


class InspectAcceptanceTests(unittest.TestCase):
    def collect_with(self, stdout):
        stub = InspectStub(stdout)
        with scratch_dir() as temporary:
            directory = Path(temporary)
            write_provenance(directory)
            with patch.object(evidence, "docker", stub):
                try:
                    report = evidence.collect("two-bot:fixture", directory)
                except BaseException as error:
                    self.fail(
                        f"collect() raised {type(error).__name__}: {error} "
                        "instead of returning collection_error"
                    )
                written = json.loads((directory / "runtime-image-evidence.json").read_text())
            self.assertEqual(written, report)
            return report, stub

    def main_with(self, stdout):
        stub = InspectStub(stdout)
        with scratch_dir() as temporary:
            directory = Path(temporary)
            write_provenance(directory)
            with patch.object(evidence, "docker", stub), patch.object(
                sys, "argv", ["runtime-image-evidence.py", "two-bot:fixture", str(directory)]
            ), contextlib.redirect_stdout(io.StringIO()) as out, contextlib.redirect_stderr(
                io.StringIO()
            ) as err:
                try:
                    evidence.main()
                except SystemExit as stop:
                    error = stop
                except BaseException as raised:
                    self.fail(
                        f"main() raised {type(raised).__name__}: {raised} "
                        "instead of SystemExit(collection_error)"
                    )
                else:
                    self.fail("main() must exit nonzero with collection_error")
            written = json.loads((directory / "runtime-image-evidence.json").read_text())
            return error, out.getvalue(), err.getvalue(), written, stub

    def assert_collection_error(self, report, stub, label):
        self.assertIs(report["complete"], False, f"report must be incomplete in case {label}")
        self.assertNotIn("filesystem", report, f"no filesystem on failed inspection in case {label}")
        self.assertIn("collection_error", report, f"no collection_error in case {label}")
        message = report["collection_error"]
        self.assertIsInstance(message, str)
        self.assertLessEqual(
            len(message), MAX_DIAGNOSTIC_LINE, f"collection_error is unbounded in case {label}"
        )
        lowered = message.lower()
        self.assertIn("inspect", lowered, f"collection_error does not name inspection in case {label}: {message!r}")
        self.assertTrue(
            any(token in lowered for token in ("unpars", "fail", "invalid", "malform")),
            f"collection_error lacks fixed vocabulary in case {label}: {message!r}",
        )
        self.assertNotIn(
            BODY_SENTINEL, message, f"collection_error echoes Docker stdout in case {label}"
        )
        self.assertEqual(
            stub.calls,
            [("image", "inspect", "two-bot:fixture")],
            f"failed inspection must stop before containers in case {label}: {stub.calls!r}",
        )

    def assert_clean_exit(self, error, out, err, written, stub, label):
        self.assertIsInstance(error, SystemExit)
        code = error.code
        self.assertIsInstance(code, str)
        self.assertEqual(code, written["collection_error"])
        self.assertLessEqual(len(code), MAX_DIAGNOSTIC_LINE)
        self.assertNotIn(BODY_SENTINEL, code + out + err)
        self.assertNotIn("Traceback", out + err, f"traceback printed in case {label}")
        self.assertIs(written["complete"], False)
        self.assertEqual(stub.calls, [("image", "inspect", "two-bot:fixture")])

    # --- malformed inspection ------------------------------------------------

    def test_empty_inspect_array_becomes_collection_error(self):
        report, stub = self.collect_with(b"[]")
        self.assert_collection_error(report, stub, "empty-array")

    def test_non_json_inspect_becomes_collection_error(self):
        for stdout in (
            b"not json " + BODY_SENTINEL.encode(),
            b"\xff\xfe" + BODY_SENTINEL.encode(),
            b"",
            b"   \n  ",
        ):
            with self.subTest(stdout=stdout[:12]):
                report, stub = self.collect_with(stdout)
                self.assert_collection_error(report, stub, f"non-json-{stdout[:12]!r}")

    def test_non_list_inspect_becomes_collection_error(self):
        for stdout in (b"null", b"{}", b'"hi"', b"123"):
            with self.subTest(stdout=stdout):
                report, stub = self.collect_with(stdout)
                self.assert_collection_error(report, stub, f"non-list-{stdout!r}")

    def test_non_object_element_becomes_collection_error(self):
        for stdout in (b"[null]", b"[123]", b'["hi"]', b"[[]]"):
            with self.subTest(stdout=stdout):
                report, stub = self.collect_with(stdout)
                self.assert_collection_error(report, stub, f"non-object-{stdout!r}")

    def test_missing_keys_become_collection_error(self):
        full = {"Id": IMAGE_ID, "Architecture": "amd64", "Os": "linux", "Config": {
            "User": "65532:65532", "Entrypoint": ["/home/nonroot/two-bot"], "Cmd": None}}
        for missing in ("Id", "Config", "Architecture", "Os"):
            with self.subTest(missing=missing):
                partial = {key: value for key, value in full.items() if key != missing}
                report, stub = self.collect_with(json.dumps([partial]).encode())
                self.assert_collection_error(report, stub, f"missing-{missing}")

    def test_config_without_get_becomes_collection_error(self):
        full = {"Id": IMAGE_ID, "Architecture": "amd64", "Os": "linux", "Config": "not-an-object"}
        report, stub = self.collect_with(json.dumps([full]).encode())
        self.assert_collection_error(report, stub, "config-not-object")

    # --- CLI exit contract ---------------------------------------------------

    def test_main_exits_clean_with_collection_error(self):
        for stdout, label in (
            (b"[]", "empty-array"),
            (b"not json " + BODY_SENTINEL.encode(), "non-json"),
            (json.dumps([{"Id": IMAGE_ID}]).encode(), "key-missing"),
        ):
            with self.subTest(label=label):
                error, out, err, written, stub = self.main_with(stdout)
                self.assert_clean_exit(error, out, err, written, stub, f"main-{label}")

    # --- passing controls ----------------------------------------------------

    def test_control_valid_inspection_returns_complete_report(self):
        image = ImageFixture()
        docker = FakeDocker(image)
        with scratch_dir() as temporary:
            directory = Path(temporary)
            write_provenance(directory)
            with patch.object(evidence, "docker", docker):
                report = evidence.collect("two-bot:fixture", directory)
            written = json.loads((directory / "runtime-image-evidence.json").read_text())
        self.assertEqual(written, report)
        self.assertIs(report["complete"], True)
        self.assertNotIn("collection_error", report)
        self.assertEqual(
            (report["schema_version"], report["image_id"], report["source_sha"]),
            (2, IMAGE_ID, SOURCE_SHA),
        )
        self.assertEqual(
            (report["architecture"], report["os"], report["configured_user"]),
            ("amd64", "linux", "65532:65532"),
        )
        rows = report["filesystem"]["installed_packages"]
        self.assertEqual(
            [(row["package"], row["version"], row["architecture"]) for row in rows],
            [
                ("libc6", "2.41-12+deb13u4", "amd64"),
                ("libgcc-s1", "14.2.0-19", "amd64"),
                ("tzdata", "2026c-0+deb13u1", "all"),
                ("zlib1g", "1:1.3.dfsg+really1.3.1-1+b1", "amd64"),
            ],
        )

    def test_control_identity_mismatch_still_raises_before_containers(self):
        image = ImageFixture()
        docker = FakeDocker(image)
        docker.metadata = [{**METADATA[0], "Id": "sha256:" + "c" * 64}]
        with scratch_dir() as temporary:
            directory = Path(temporary)
            write_provenance(directory)
            with patch.object(evidence, "docker", docker), self.assertRaisesRegex(
                ValueError, "identity"
            ):
                evidence.collect("two-bot:fixture", directory)
        self.assertEqual(len(docker.calls), 1)

    def test_control_invalid_provenance_stops_before_docker(self):
        with scratch_dir() as temporary:
            directory = Path(temporary)
            (directory / "image-id.txt").write_text(IMAGE_ID + "\n")
            (directory / "source-sha.txt").write_text("unknown\n")
            with patch.object(evidence, "docker") as docker, self.assertRaises(ValueError):
                evidence.collect("two-bot:fixture", directory)
            docker.assert_not_called()


if __name__ == "__main__":
    unittest.main()
