"""Protected acceptance: release-sbom-target refuses oversized, ambiguous and missing intake.

``release-sbom-target.py`` takes unbounded ``gh api`` intake with a bare
``json.loads`` and ``KeyError`` tracebacks on missing env, so hostile or
absent input either parses last-wins or escapes as a traceback:

* over-cap ``gh`` responses are fully buffered and parsed (no byte cap);
* duplicate JSON keys last-wins past tag resolution;
* ``NaN``/``Infinity``/out-of-range floats parse and flow into resolution;
* missing ``GH_REPO``/``GITHUB_SHA``/``GITHUB_OUTPUT``/``RELEASE_TAG``/
  ``EXPECTED_SHA`` escape as ``KeyError`` tracebacks.

Sibling scope: PR #1469 owns transport/shape refusal in ``github_object``
(``Release tag lookup failed`` / ``Release tag response has invalid shape``)
plus ``ValueError``-to-exit-1 at main. This file is explicitly the remainder
and must stay green on top of it: every fixture below is shape-valid small
or large input that #1469 still accepts (or still tracebacks on), and every
control pins behavior #1469 preserves byte-identical.

The fix must refuse every mode with fixed vocabulary, no traceback, no echo
of response bytes, and exit 1. Exact contract:

* ``OVER_CAP`` when a ``gh`` response exceeds the byte cap;
* ``DUP_KEYS`` when a ``gh`` response carries duplicate JSON object keys;
* ``BAD_CONST`` when a ``gh`` response carries a non-finite number;
* ``"<VAR> is not set"`` when required env ``VAR`` is missing.

Over-cap fixtures are 2 MiB: the builder's transport cap sits at or below
that ceiling while real ``gh`` responses (< 1 KiB) keep passing.

RED expectations on the current code: over-cap/duplicate/constant responses
are buffered and parsed (tag resolution succeeds); missing env leaks
``KeyError`` tracebacks.

Offline only: ``gh`` is stubbed in-process or faked on ``PATH``; no network.
Builder must not edit this file; build on the pushed head.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_release_sbom_target_intake_refusal_acceptance.py' -v
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

SCRIPT = Path(__file__).with_name("release-sbom-target.py")
spec = importlib.util.spec_from_file_location("release_sbom_target", SCRIPT)
target = importlib.util.module_from_spec(spec)
spec.loader.exec_module(target)


OVER_CAP = "Release tag response exceeds the byte cap"
DUP_KEYS = "Release tag response carries duplicate object keys"
BAD_CONST = "Release tag response carries a non-finite number"
CANARY = "RELEASE-TARGET-CANARY-NEVER-ECHO-2PL6"

BIG_RESPONSE = 2 << 20
COMMIT_SHA = "a" * 40
TAG_SHA = "b" * 40
OTHER_SHA = "c" * 40

# The big-response branch concatenates at runtime so the 2 MiB body never
# crosses an execve environment-variable limit (MAX_ARG_STRLEN is 128 KiB).
_FAKE_GH_BIG_BODY = (
    '\'{"object": {"sha": "' + COMMIT_SHA + '", "type": "commit"}, "pad": "\' + "'
    + CANARY + '" + "x" * ' + str(BIG_RESPONSE) + ' + \'"}\'')
FAKE_GH = (
    "#!/usr/bin/env python3\n"
    "import os\n"
    "import sys\n"
    'if os.environ.get("FAKE_GH_BIG") == "1":\n'
    "    sys.stdout.write(" + _FAKE_GH_BIG_BODY + ")\n"
    "    sys.exit(0)\n"
    'arg = sys.argv[-1]\n'
    'if "ref/tags/" in arg:\n'
    '    out = os.environ.get("FAKE_GH_REF", "{}")\n'
    'elif "/tags/" in arg:\n'
    '    out = os.environ.get("FAKE_GH_TAGS", "{}")\n'
    "else:\n"
    '    out = "{}"\n'
    "sys.stdout.write(out)\n")


def scratch_dir():
    return (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
            or os.environ.get("PAPERCLIP_SCRATCH_DIR")
            or os.environ.get("RUNNER_TEMP"))


def ref_response(sha=COMMIT_SHA, type="commit"):
    return json.dumps({"object": {"sha": sha, "type": type}})


class ReleaseSbomTargetIntakeRefusalAcceptance(unittest.TestCase):
    def resolve(self, tag="v1.2.3", ref=None, tags=None):
        def fake(command, **kwargs):
            endpoint = command[-1]
            if "ref/tags/" in endpoint:
                return ref if ref is not None else ref_response()
            if "/tags/" in endpoint:
                return tags if tags is not None else ref_response()
            raise AssertionError(f"unexpected gh endpoint: {endpoint}")

        with patch.object(target.subprocess, "check_output", side_effect=fake), \
                patch.dict(os.environ, {"GH_REPO": "o/r"}):
            return target.resolve_tag(tag)

    def run_cli(self, extra_env=None, args=(), with_gh=True):
        directory = tempfile.mkdtemp(dir=scratch_dir())
        self.addCleanup(shutil.rmtree, directory, True)
        env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
        if with_gh:
            bindir = Path(directory) / "bin"
            bindir.mkdir()
            gh = bindir / "gh"
            gh.write_text(FAKE_GH)
            gh.chmod(gh.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
            env["PATH"] = str(bindir) + os.pathsep + env.get("PATH", "")
        env.update(extra_env or {})
        output = Path(directory) / "output.txt"
        result = subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            capture_output=True, text=True, env=env, timeout=60)
        return result, output

    def assert_cli_refusal(self, result, message, label):
        combined = result.stdout + result.stderr
        self.assertEqual(result.returncode, 1, f"over-accepted input in case {label}")
        self.assertNotIn("Traceback", combined, f"traceback leaked in case {label}")
        self.assertIn(message, combined, f"fixed refusal missing in case {label}")
        self.assertNotIn(CANARY, combined, f"response echoed in case {label}")

    # --- over-cap gh responses (buffered and parsed today) ---------------------

    def big_valid_response(self):
        body = json.dumps({"object": {"sha": COMMIT_SHA, "type": "commit"},
                           "pad": CANARY + "x" * BIG_RESPONSE})
        self.assertGreaterEqual(len(body), BIG_RESPONSE)
        return body

    def test_over_cap_gh_response_refuses(self):
        with self.assertRaises(ValueError) as caught:
            self.resolve(ref=self.big_valid_response())
        self.assertEqual(str(caught.exception), OVER_CAP)

    def test_over_cap_nested_tag_fetch_refuses(self):
        ref = ref_response(sha=TAG_SHA, type="tag")
        with self.assertRaises(ValueError) as caught:
            self.resolve(ref=ref, tags=self.big_valid_response())
        self.assertEqual(str(caught.exception), OVER_CAP)

    def test_cli_over_cap_gh_response_refuses_without_traceback(self):
        directory = tempfile.mkdtemp(dir=scratch_dir())
        self.addCleanup(shutil.rmtree, directory, True)
        result, _ = self.run_cli(extra_env={
            "GH_REPO": "o/r", "DRY_RUN": "", "RELEASE_TAG": "v1.2.3",
            "GITHUB_SHA": OTHER_SHA,
            "GITHUB_OUTPUT": os.path.join(directory, "o"),
            "FAKE_GH_BIG": "1"})
        self.assert_cli_refusal(result, OVER_CAP, "transport")

    # --- duplicate JSON keys (last-wins today) ---------------------------------

    def test_duplicate_keys_refuse(self):
        body = '{"object": {"sha": "%s", "type": "commit", "sha": "%s"}}' % (
            COMMIT_SHA, COMMIT_SHA)
        with self.assertRaises(ValueError) as caught:
            self.resolve(ref=body)
        self.assertEqual(str(caught.exception), DUP_KEYS)

    def test_nested_tag_fetch_duplicate_keys_refuse(self):
        ref = ref_response(sha=TAG_SHA, type="tag")
        body = '{"object": {"sha": "%s", "type": "commit", "sha": "%s"}}' % (
            COMMIT_SHA, COMMIT_SHA)
        with self.assertRaises(ValueError) as caught:
            self.resolve(ref=ref, tags=body)
        self.assertEqual(str(caught.exception), DUP_KEYS)

    def test_cli_duplicate_keys_refuse_without_traceback(self):
        body = ('{"object": {"sha": "%s", "type": "commit"}, '
                '"note": "%s", "note": "%s"}') % (COMMIT_SHA, CANARY, CANARY)
        directory = tempfile.mkdtemp(dir=scratch_dir())
        self.addCleanup(shutil.rmtree, directory, True)
        result, _ = self.run_cli(extra_env={
            "GH_REPO": "o/r", "DRY_RUN": "", "RELEASE_TAG": "v1.2.3",
            "GITHUB_SHA": OTHER_SHA,
            "GITHUB_OUTPUT": os.path.join(directory, "o"),
            "FAKE_GH_REF": body})
        self.assert_cli_refusal(result, DUP_KEYS, "duplicate-keys")

    # --- out-of-range constants (parsed today) -----------------------------------

    def test_out_of_range_constant_refuses(self):
        for constant in ("NaN", "Infinity", "1e1000"):
            with self.subTest(constant=constant):
                body = ('{"object": {"sha": "%s", "type": "commit"}, '
                        '"pad": %s}') % (COMMIT_SHA, constant)
                with self.assertRaises(ValueError) as caught:
                    self.resolve(ref=body)
                self.assertEqual(str(caught.exception), BAD_CONST)

    def test_cli_out_of_range_constant_refuses_without_traceback(self):
        body = ('{"object": {"sha": "%s", "type": "commit"}, "pad": NaN, '
                '"note": "%s"}') % (COMMIT_SHA, CANARY)
        directory = tempfile.mkdtemp(dir=scratch_dir())
        self.addCleanup(shutil.rmtree, directory, True)
        result, _ = self.run_cli(extra_env={
            "GH_REPO": "o/r", "DRY_RUN": "", "RELEASE_TAG": "v1.2.3",
            "GITHUB_SHA": OTHER_SHA,
            "GITHUB_OUTPUT": os.path.join(directory, "o"),
            "FAKE_GH_REF": body})
        self.assert_cli_refusal(result, BAD_CONST, "non-finite")

    # --- missing env (KeyError tracebacks today) ---------------------------------

    def base_env(self, directory):
        return {"DRY_RUN": "", "RELEASE_TAG": "v1.2.3", "GITHUB_SHA": OTHER_SHA,
                "GITHUB_OUTPUT": os.path.join(directory, "o"), "GH_REPO": "o/r"}

    def test_cli_missing_gh_repo_refuses_without_traceback(self):
        directory = tempfile.mkdtemp(dir=scratch_dir())
        self.addCleanup(shutil.rmtree, directory, True)
        env = self.base_env(directory)
        del env["GH_REPO"]
        saved = os.environ.pop("GH_REPO", None)
        try:
            result, _ = self.run_cli(extra_env=env)
        finally:
            if saved is not None:
                os.environ["GH_REPO"] = saved
        combined = result.stdout + result.stderr
        self.assertEqual(result.returncode, 1, "missing GH_REPO accepted")
        self.assertNotIn("Traceback", combined, "traceback leaked")
        self.assertIn("GH_REPO is not set", combined, "fixed refusal missing")

    def test_cli_missing_dry_run_env_refuses_without_traceback(self):
        for var in ("GITHUB_SHA", "GITHUB_OUTPUT"):
            with self.subTest(var=var):
                directory = tempfile.mkdtemp(dir=scratch_dir())
                self.addCleanup(shutil.rmtree, directory, True)
                env = {"DRY_RUN": "true", "RELEASE_TAG": "",
                       "GITHUB_SHA": OTHER_SHA,
                       "GITHUB_OUTPUT": os.path.join(directory, "o")}
                del env[var]
                saved = os.environ.pop(var, None)
                try:
                    result, _ = self.run_cli(extra_env=env, with_gh=False)
                finally:
                    if saved is not None:
                        os.environ[var] = saved
                combined = result.stdout + result.stderr
                self.assertEqual(result.returncode, 1, f"missing {var} accepted")
                self.assertNotIn("Traceback", combined, "traceback leaked")
                self.assertIn(f"{var} is not set", combined, "fixed refusal missing")

    def test_cli_missing_verify_source_env_refuses_without_traceback(self):
        for var in ("RELEASE_TAG", "EXPECTED_SHA"):
            with self.subTest(var=var):
                directory = tempfile.mkdtemp(dir=scratch_dir())
                self.addCleanup(shutil.rmtree, directory, True)
                source = Path(directory) / "source-sha.txt"
                source.write_text(COMMIT_SHA + "\n")
                env = {"RELEASE_TAG": "v1.2.3", "EXPECTED_SHA": COMMIT_SHA}
                del env[var]
                saved = os.environ.pop(var, None)
                try:
                    result, _ = self.run_cli(
                        extra_env=env, with_gh=False,
                        args=("--verify-source", str(source)))
                finally:
                    if saved is not None:
                        os.environ[var] = saved
                combined = result.stdout + result.stderr
                self.assertEqual(result.returncode, 1, f"missing {var} accepted")
                self.assertNotIn("Traceback", combined, "traceback leaked")
                self.assertIn(f"{var} is not set", combined, "fixed refusal missing")

    # --- passing controls (green now and on top of #1469) ------------------------

    def test_control_tag_to_commit_resolves(self):
        self.assertEqual(self.resolve(), COMMIT_SHA)

    def test_control_annotated_tag_peels_to_commit(self):
        ref = ref_response(sha=TAG_SHA, type="tag")
        self.assertEqual(self.resolve(ref=ref, tags=ref_response()), COMMIT_SHA)

    def test_control_dry_run_selects_source_sha(self):
        self.assertEqual(target.select_target("true", "", COMMIT_SHA),
                         {"ref": COMMIT_SHA, "tag": ""})

    def test_control_semantic_messages_unchanged(self):
        with self.assertRaises(ValueError) as caught:
            self.resolve(tag="main")
        self.assertEqual(str(caught.exception),
                         "Publication requires a stable vX.Y.Z release tag")
        with self.assertRaises(ValueError) as caught:
            target.select_target("true", "", "zzz")
        self.assertEqual(str(caught.exception), "Dry-run requires the workflow source SHA")

    def test_cli_control_valid_dry_run_byte_identical(self):
        directory = tempfile.mkdtemp(dir=scratch_dir())
        self.addCleanup(shutil.rmtree, directory, True)
        output = os.path.join(directory, "o")
        result, _ = self.run_cli(extra_env={
            "DRY_RUN": "true", "RELEASE_TAG": "", "GITHUB_SHA": COMMIT_SHA,
            "GITHUB_OUTPUT": output}, with_gh=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        self.assertEqual(Path(output).read_text(), f"ref={COMMIT_SHA}\ntag=\n")

    def test_cli_control_valid_tag_byte_identical(self):
        directory = tempfile.mkdtemp(dir=scratch_dir())
        self.addCleanup(shutil.rmtree, directory, True)
        output = os.path.join(directory, "o")
        result, _ = self.run_cli(extra_env={
            "GH_REPO": "o/r", "DRY_RUN": "", "RELEASE_TAG": "v1.2.3",
            "GITHUB_SHA": OTHER_SHA, "GITHUB_OUTPUT": output,
            "FAKE_GH_REF": ref_response()})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(Path(output).read_text(), f"ref={COMMIT_SHA}\ntag=v1.2.3\n")


if __name__ == "__main__":
    unittest.main()
