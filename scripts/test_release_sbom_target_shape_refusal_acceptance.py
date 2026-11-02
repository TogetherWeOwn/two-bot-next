"""Protected acceptance: release-sbom-target refuses unusable gh output.

``resolve_tag`` indexes ``["object"]`` on whatever ``gh api`` returns, and the
CLI has no error handling at all, so a missing-shape response escapes as
``KeyError``/``AttributeError``/``TypeError`` (or a ``ValueError`` traceback
for the semantic checks). The fix must refuse with fixed vocabulary only:

* ``SHAPE_REASON`` when the gh response is not JSON, or is JSON without an
  ``{"object": {...}}`` dict (top-level non-dict, missing ``object``, or a
  non-dict ``object``), on either the ref or the nested tag fetch;
* ``LOOKUP_REASON`` when the ``gh`` subprocess itself fails (nonzero exit,
  timeout, or ``OSError``);
* the pre-existing semantic messages byte-identical (bad tag, bad SHA,
  non-commit type, nesting bound, dry-run SHA, source mismatch, moved tag),
  including non-string ``sha`` fields, which must refuse as
  ``"Invalid release tag object SHA"`` instead of raising ``TypeError``.

Function level raises ``ValueError`` with exactly the fixed message. CLI level
exits nonzero with the fixed message on stdout or stderr, no traceback, and
no echo of the gh response.

Passing controls pin the valid-input behavior byte-for-byte (same resolved
refs, same ``GITHUB_OUTPUT`` bytes, same semantic messages) so the fix cannot
trade one refusal for another.

Offline only: ``gh`` is stubbed in-process or faked on ``PATH``; no network.
Builder must not edit this file; build on the pushed head.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_release_sbom_target_shape_refusal_acceptance.py' -v
"""

import importlib.util
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "release_sbom_target", Path(__file__).with_name("release-sbom-target.py"))
target = importlib.util.module_from_spec(spec)
spec.loader.exec_module(target)


SHAPE_REASON = "Release tag response has invalid shape"
LOOKUP_REASON = "Release tag lookup failed"
INVALID_SHA = "Invalid release tag object SHA"
BAD_TAG = "Publication requires a stable vX.Y.Z release tag"
NOT_COMMIT = "Release tag does not resolve to a commit"
NESTING_BOUND = "Release tag nesting exceeds the resolution bound"
DRY_RUN_SHA = "Dry-run requires the workflow source SHA"
SOURCE_DIFFERS = "SBOM source differs from the selected release commit"
TAG_MOVED = "Release tag moved since SBOM source selection"
CANARY = "SBOM-CANARY-NEVER-ECHO-9X2W"

COMMIT_SHA = "a" * 40
TAG_SHA = "b" * 40
OTHER_SHA = "c" * 40

FAKE_GH = """#!/usr/bin/env python3
import os
import sys
arg = sys.argv[-1]
if "ref/tags/" in arg:
    out = os.environ.get("FAKE_GH_REF", "{}")
elif "/tags/" in arg:
    out = os.environ.get("FAKE_GH_TAGS", "{}")
else:
    out = "{}"
code = int(os.environ.get("FAKE_GH_EXIT", "0"))
if code == 0:
    sys.stdout.write(out)
else:
    sys.stderr.write("gh failed")
sys.exit(code)
"""


def scratch_dir():
    return (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
            or os.environ.get("PAPERCLIP_SCRATCH_DIR")
            or os.environ.get("RUNNER_TEMP"))


def ref_response(sha=COMMIT_SHA, type="commit"):
    return json.dumps({"object": {"sha": sha, "type": type}})


class ShapeRefusalAcceptanceTests(unittest.TestCase):
    def check_output_for(self, ref=None, tags=None, error=None):
        def fake(command, **kwargs):
            if error is not None:
                raise error
            endpoint = command[-1]
            if "ref/tags/" in endpoint:
                return ref if ref is not None else ref_response()
            if "/tags/" in endpoint:
                return tags if tags is not None else ref_response()
            raise AssertionError(f"unexpected gh endpoint: {endpoint}")

        return fake

    def resolve(self, tag="v1.2.3", **kwargs):
        with patch.object(target.subprocess, "check_output",
                           side_effect=self.check_output_for(**kwargs)), \
                patch.dict(os.environ, {"GH_REPO": "o/r"}):
            return target.resolve_tag(tag)

    # --- missing-shape gh responses --------------------------------------

    def test_non_json_gh_response_refuses_with_fixed_vocabulary(self):
        for body in ("not json " + CANARY, "", "<html>" + CANARY):
            with self.subTest(body=body[:20]):
                with patch.object(target.subprocess, "check_output",
                                   return_value=body), \
                        patch.dict(os.environ, {"GH_REPO": "o/r"}):
                    with self.assertRaises(ValueError) as caught:
                        target.resolve_tag("v1.2.3")
                self.assertEqual(str(caught.exception), SHAPE_REASON)
                self.assertNotIn(CANARY, str(caught.exception))

    def test_missing_shape_gh_response_refuses_with_fixed_vocabulary(self):
        cases = (
            "[]",
            '"just a string"',
            "null",
            "123",
            "{}",
            json.dumps({"object": None, "note": CANARY}),
            json.dumps({"object": "sha-string"}),
            json.dumps({"object": ["sha-list"]}),
            json.dumps({"object": 42}),
        )
        for body in cases:
            with self.subTest(body=body[:30]):
                with self.assertRaises(ValueError) as caught:
                    self.resolve(ref=body)
                self.assertEqual(str(caught.exception), SHAPE_REASON)
                self.assertNotIn(CANARY, str(caught.exception))

    def test_nested_tag_fetch_missing_shape_refuses_with_fixed_vocabulary(self):
        ref = ref_response(sha=TAG_SHA, type="tag")
        for body in ("not json " + CANARY, "{}", json.dumps({"object": None})):
            with self.subTest(body=body[:20]):
                with self.assertRaises(ValueError) as caught:
                    self.resolve(ref=ref, tags=body)
                self.assertEqual(str(caught.exception), SHAPE_REASON)
                self.assertNotIn(CANARY, str(caught.exception))

    def test_non_string_sha_refuses_as_invalid_sha_not_type_error(self):
        for sha in (123, None, [], {}, True):
            with self.subTest(sha=sha):
                body = json.dumps({"object": {"sha": sha, "type": "commit"}})
                with self.assertRaises(ValueError) as caught:
                    self.resolve(ref=body)
                self.assertEqual(str(caught.exception), INVALID_SHA)

    # --- gh subprocess failures -------------------------------------------

    def test_gh_subprocess_failure_refuses_with_fixed_vocabulary(self):
        errors = (
            subprocess.CalledProcessError(1, ["gh", "api"], stderr="rate limited " + CANARY),
            subprocess.TimeoutExpired(["gh", "api"], 30),
            OSError("gh not found " + CANARY),
            FileNotFoundError("gh"),
        )
        for error in errors:
            with self.subTest(error=type(error).__name__):
                with patch.object(target.subprocess, "check_output",
                                   side_effect=error), \
                        patch.dict(os.environ, {"GH_REPO": "o/r"}):
                    with self.assertRaises(ValueError) as caught:
                        target.resolve_tag("v1.2.3")
                self.assertEqual(str(caught.exception), LOOKUP_REASON)
                self.assertNotIn(CANARY, str(caught.exception))

    # --- CLI surface: nonzero exit, no traceback -----------------------------

    def run_cli(self, ref=None, tags=None, exit_code="0", extra_env=None, args=()):
        # mkdtemp (not a TemporaryDirectory context): the caller reads the
        # GITHUB_OUTPUT file after this helper returns.
        directory = tempfile.mkdtemp(dir=scratch_dir())
        self.addCleanup(shutil.rmtree, directory, True)
        bindir = Path(directory) / "bin"
        bindir.mkdir()
        gh = bindir / "gh"
        gh.write_text(FAKE_GH)
        gh.chmod(gh.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
        output = Path(directory) / "output.txt"
        env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1",
                   PATH=str(bindir) + os.pathsep + os.environ.get("PATH", ""),
                   GH_REPO="o/r", GITHUB_OUTPUT=str(output),
                   FAKE_GH_EXIT=exit_code)
        if ref is not None:
            env["FAKE_GH_REF"] = ref
        if tags is not None:
            env["FAKE_GH_TAGS"] = tags
        env.update(extra_env or {})
        result = subprocess.run(
            [sys.executable, str(Path(__file__).with_name("release-sbom-target.py")), *args],
            capture_output=True, text=True, env=env, timeout=60)
        return result, output

    def assert_cli_refusal(self, result, reason, label):
        combined = result.stdout + result.stderr
        self.assertNotEqual(result.returncode, 0, f"unusable input accepted in case {label}")
        self.assertNotIn("Traceback", combined, f"traceback leaked in case {label}")
        self.assertIn(reason, combined, f"fixed refusal missing in case {label}")
        self.assertNotIn(CANARY, combined, f"gh response echoed in case {label}")

    def test_cli_missing_shape_refuses_without_traceback(self):
        for body in ("not json " + CANARY,
                     json.dumps({"note": CANARY}),
                     json.dumps({"object": None})):
            with self.subTest(body=body[:20]):
                result, _ = self.run_cli(
                    ref=body,
                    extra_env={"DRY_RUN": "", "RELEASE_TAG": "v1.2.3",
                               "GITHUB_SHA": OTHER_SHA})
                self.assert_cli_refusal(result, SHAPE_REASON, f"shape:{body[:20]}")

    def test_cli_lookup_failure_refuses_without_traceback(self):
        result, _ = self.run_cli(
            exit_code="1",
            extra_env={"DRY_RUN": "", "RELEASE_TAG": "v1.2.3",
                       "GITHUB_SHA": OTHER_SHA})
        self.assert_cli_refusal(result, LOOKUP_REASON, "lookup")

    def test_cli_semantic_refusals_refuse_without_traceback(self):
        cases = (
            ({"DRY_RUN": "", "RELEASE_TAG": "main", "GITHUB_SHA": OTHER_SHA}, BAD_TAG),
            ({"DRY_RUN": "", "RELEASE_TAG": "v1", "GITHUB_SHA": OTHER_SHA}, BAD_TAG),
            ({"DRY_RUN": "true", "RELEASE_TAG": "", "GITHUB_SHA": "zzz"}, DRY_RUN_SHA),
        )
        for env, reason in cases:
            with self.subTest(reason=reason):
                result, _ = self.run_cli(extra_env=env)
                self.assert_cli_refusal(result, reason, reason)

    def test_cli_verify_source_mismatch_refuses_without_traceback(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            source = Path(directory) / "source-sha.txt"
            source.write_text(OTHER_SHA + "\n")
            result, _ = self.run_cli(
                extra_env={"RELEASE_TAG": "v1.2.3", "EXPECTED_SHA": COMMIT_SHA},
                args=("--verify-source", str(source)))
            self.assert_cli_refusal(result, SOURCE_DIFFERS, "verify-source")

    # --- passing controls: valid behavior byte-identical ----------------------

    def test_control_tag_to_commit_resolves(self):
        self.assertEqual(self.resolve(), COMMIT_SHA)

    def test_control_annotated_tag_peels_to_commit(self):
        ref = ref_response(sha=TAG_SHA, type="tag")
        nested = ref_response(sha=COMMIT_SHA, type="commit")
        self.assertEqual(self.resolve(ref=ref, tags=nested), COMMIT_SHA)

    def test_control_dry_run_selects_source_sha(self):
        self.assertEqual(target.select_target("true", "", COMMIT_SHA),
                         {"ref": COMMIT_SHA, "tag": ""})

    def test_control_semantic_messages_unchanged(self):
        with self.assertRaises(ValueError) as caught:
            self.resolve(tag="v1")
        self.assertEqual(str(caught.exception), BAD_TAG)
        with self.assertRaises(ValueError) as caught:
            target.select_target("true", "", "zzz")
        self.assertEqual(str(caught.exception), DRY_RUN_SHA)
        with self.assertRaises(ValueError) as caught:
            self.resolve(ref=ref_response(type="blob"))
        self.assertEqual(str(caught.exception), NOT_COMMIT)
        with self.assertRaises(ValueError) as caught:
            self.resolve(ref=ref_response(sha="ZZZ", type="commit"))
        self.assertEqual(str(caught.exception), INVALID_SHA)
        with self.assertRaises(ValueError) as caught:
            self.resolve(ref=ref_response(sha=TAG_SHA, type="tag"),
                          tags=ref_response(sha=TAG_SHA, type="tag"))
        self.assertEqual(str(caught.exception), NESTING_BOUND)

    def test_cli_control_valid_dry_run_byte_identical(self):
        result, output = self.run_cli(
            extra_env={"DRY_RUN": "true", "RELEASE_TAG": "",
                       "GITHUB_SHA": COMMIT_SHA})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        self.assertEqual(output.read_text(), f"ref={COMMIT_SHA}\ntag=\n")

    def test_cli_control_valid_tag_byte_identical(self):
        result, output = self.run_cli(
            ref=ref_response(),
            extra_env={"DRY_RUN": "", "RELEASE_TAG": "v1.2.3",
                       "GITHUB_SHA": OTHER_SHA})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output.read_text(), f"ref={COMMIT_SHA}\ntag=v1.2.3\n")

    def test_cli_control_valid_annotated_tag_byte_identical(self):
        result, output = self.run_cli(
            ref=ref_response(sha=TAG_SHA, type="tag"),
            tags=ref_response(),
            extra_env={"DRY_RUN": "", "RELEASE_TAG": "v1.2.3",
                       "GITHUB_SHA": OTHER_SHA})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output.read_text(), f"ref={COMMIT_SHA}\ntag=v1.2.3\n")

    def test_cli_control_verify_source_valid_passes_silently(self):
        with tempfile.TemporaryDirectory(dir=scratch_dir()) as directory:
            source = Path(directory) / "source-sha.txt"
            source.write_text(COMMIT_SHA + "\n")
            result, _ = self.run_cli(
                ref=ref_response(),
                extra_env={"RELEASE_TAG": "v1.2.3", "EXPECTED_SHA": COMMIT_SHA},
                args=("--verify-source", str(source)))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout + result.stderr, "")


if __name__ == "__main__":
    unittest.main()
