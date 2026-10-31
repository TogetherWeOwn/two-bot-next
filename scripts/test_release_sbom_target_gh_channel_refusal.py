"""Pin the gh output channels behind release-sbom-target's fixed refusals.

Three channels escape the shape/lookup vocabulary when ``gh`` misbehaves:

* ``gh`` failing (nonzero exit) writes HTTP diagnostics to stderr, which
  ``subprocess.check_output`` inherits unless redirected, so service text
  lands in the log ahead of the fixed lookup refusal;
* ``gh`` succeeding with non-UTF-8 stdout raises ``UnicodeDecodeError``
  inside ``check_output(text=True)``, before ``json.loads`` ever runs, so
  the raw codec message escapes instead of the fixed shape refusal;
* ``gh`` succeeding with an over-limit integer token (past CPython's
  4300-digit conversion cap, still far under the 1 MiB byte cap) raises a
  plain ``ValueError`` inside ``json.loads``, not ``JSONDecodeError``, so
  the raw interpreter limit text escapes instead of the fixed shape
  refusal.

All refuse with the existing fixed reasons and never echo gh output.
Offline only: ``gh`` is faked on ``PATH``; no network, no credentials.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_release_sbom_target_gh_channel_refusal.py' -v
"""

import importlib.util
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
CANARY = "SBOM-CANARY-NEVER-ECHO-9X2W"
OTHER_SHA = "c" * 40
# Past CPython's 4300-digit int conversion cap, far under the 1 MiB byte cap.
GIANT_INT = "9" * 5000
VALID_SHA = "a" * 40

FAKE_GH = """#!/usr/bin/env python3
import os
import sys
raw_hex = os.environ.get("FAKE_GH_RAW_HEX", "")
if raw_hex:
    sys.stdout.buffer.write(bytes.fromhex(raw_hex))
    sys.exit(0)
code = int(os.environ.get("FAKE_GH_EXIT", "0"))
if code == 0:
    sys.stdout.write(os.environ.get("FAKE_GH_REF", "{}"))
else:
    sys.stderr.write(os.environ.get("FAKE_GH_STDERR", "gh failed"))
sys.exit(code)
"""


def scratch_dir():
    return (os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
            or os.environ.get("PAPERCLIP_SCRATCH_DIR")
            or os.environ.get("RUNNER_TEMP"))


class GhChannelRefusalTests(unittest.TestCase):
    def run_cli(self, extra_env):
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
                   DRY_RUN="", RELEASE_TAG="v1.2.3", GITHUB_SHA=OTHER_SHA)
        env.update(extra_env)
        return subprocess.run(
            [sys.executable, str(Path(__file__).with_name("release-sbom-target.py"))],
            capture_output=True, text=True, env=env, timeout=60)

    def test_cli_gh_stderr_never_precedes_fixed_lookup_refusal(self):
        result = self.run_cli({"FAKE_GH_EXIT": "1", "FAKE_GH_STDERR": CANARY})
        combined = result.stdout + result.stderr
        self.assertEqual(result.returncode, 1)
        self.assertIn(LOOKUP_REASON, combined)
        self.assertNotIn("Traceback", combined)
        self.assertNotIn(CANARY, combined)

    def test_cli_non_utf8_gh_stdout_refuses_as_invalid_shape(self):
        result = self.run_cli({"FAKE_GH_RAW_HEX": "fffe"})
        combined = result.stdout + result.stderr
        self.assertEqual(result.returncode, 1)
        self.assertIn(SHAPE_REASON, combined)
        self.assertNotIn("Traceback", combined)
        self.assertNotIn("codec", combined)

    def test_non_utf8_gh_stdout_refuses_as_invalid_shape(self):
        with patch.object(target.subprocess, "check_output",
                           side_effect=UnicodeDecodeError("utf-8", b"\xff", 0, 1, "invalid start byte")), \
                patch.dict(os.environ, {"GH_REPO": "o/r"}):
            with self.assertRaises(ValueError) as caught:
                target.resolve_tag("v1.2.3")
        self.assertEqual(str(caught.exception), SHAPE_REASON)

    def test_giant_int_sha_refuses_as_invalid_shape(self):
        raw = '{"object": {"sha": ' + GIANT_INT + ', "type": "commit"}}'
        with patch.object(target.subprocess, "check_output", return_value=raw), \
                patch.dict(os.environ, {"GH_REPO": "o/r"}):
            with self.assertRaises(ValueError) as caught:
                target.resolve_tag("v1.2.3")
        self.assertEqual(str(caught.exception), SHAPE_REASON)

    def test_giant_int_extra_field_refuses_as_invalid_shape(self):
        raw = ('{"object": {"sha": "' + VALID_SHA + '", "type": "commit", '
               '"extra": ' + GIANT_INT + '}}')
        with patch.object(target.subprocess, "check_output", return_value=raw), \
                patch.dict(os.environ, {"GH_REPO": "o/r"}):
            with self.assertRaises(ValueError) as caught:
                target.resolve_tag("v1.2.3")
        self.assertEqual(str(caught.exception), SHAPE_REASON)

    def test_cli_giant_int_sha_refuses_as_invalid_shape(self):
        result = self.run_cli(
            {"FAKE_GH_REF": '{"object": {"sha": ' + GIANT_INT + ', "type": "commit"}}'})
        combined = result.stdout + result.stderr
        self.assertEqual(result.returncode, 1)
        self.assertIn(SHAPE_REASON, combined)
        self.assertNotIn("Traceback", combined)
        self.assertNotIn("Exceeds the limit", combined)
        self.assertNotIn(GIANT_INT, combined)

    def test_cli_giant_int_extra_field_refuses_as_invalid_shape(self):
        result = self.run_cli(
            {"FAKE_GH_REF": '{"object": {"sha": "' + VALID_SHA + '", "type": "commit", '
                            '"extra": ' + GIANT_INT + '}}'})
        combined = result.stdout + result.stderr
        self.assertEqual(result.returncode, 1)
        self.assertIn(SHAPE_REASON, combined)
        self.assertNotIn("Traceback", combined)
        self.assertNotIn("Exceeds the limit", combined)
        self.assertNotIn(GIANT_INT, combined)


if __name__ == "__main__":
    unittest.main()
