"""Execute the release attach step offline with a stub `gh`; never upload."""

import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
SOURCE_SHA = "a" * 40
TAG_OBJECT_SHA = "b" * 40
MOVED_SHA = "c" * 40

GH_STUB = """#!/bin/sh
# Fake gh: ref/tags and tags lookups print "<type>\\n<sha>" from the scenario
# environment; a release upload just drops a marker in its second argument's
# directory so the test can prove whether the upload ran.
case "$1 $2" in
  "api repos/test/repo/git/ref/tags/v1.2.3")
    printf '%s\\n%s\\n' "$FAKE_REF_TYPE" "$FAKE_REF_SHA" ;;
  "api repos/test/repo/git/tags/"*)
    printf '%s\\n%s\\n' "$FAKE_TAG_TYPE" "$FAKE_TAG_SHA" ;;
  "release upload")
    echo uploaded > uploaded.marker ;;
  *)
    echo "unexpected gh call: $*" >&2; exit 9 ;;
esac
"""


class ReleaseAttachTagGuardTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        workflow = yaml.safe_load(
            (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        )
        steps = workflow["jobs"]["attach-sbom"]["steps"]
        cls.shell = next(step["run"] for step in steps if "run" in step)

    def run_attach(self, ref_type, ref_sha, tag_type="", tag_sha="", source_sha=SOURCE_SHA):
        with tempfile.TemporaryDirectory() as directory:
            workdir = Path(directory)
            stub = workdir / "bin"
            stub.mkdir()
            gh = stub / "gh"
            gh.write_text(GH_STUB)
            gh.chmod(0o755)
            sbom = workdir / "sbom"
            sbom.mkdir()
            (sbom / "source-sha.txt").write_text(source_sha + "\n")
            (sbom / "image-id.txt").write_text("sha256:fixture\n")
            (sbom / "fixture.cdx.json").write_text("{}\n")
            checksums = "".join(
                f"{hashlib.sha256((sbom / name).read_bytes()).hexdigest()}  {name}\n"
                for name in ("fixture.cdx.json", "source-sha.txt", "image-id.txt")
            )
            (sbom / "SHA256SUMS").write_text(checksums)
            env = {
                "PATH": f"{stub}:{os.defpath}:/usr/bin:/bin",
                "GH_REPO": "test/repo",
                "GH_TOKEN": "fixture",
                "RELEASE_TAG": "v1.2.3",
                "EXPECTED_SHA": SOURCE_SHA,
                "FAKE_REF_TYPE": ref_type,
                "FAKE_REF_SHA": ref_sha,
                "FAKE_TAG_TYPE": tag_type,
                "FAKE_TAG_SHA": tag_sha,
            }
            proc = subprocess.run(
                ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", self.shell],
                cwd=workdir, env=env, capture_output=True, text=True, timeout=10,
            )
            return proc, (workdir / "uploaded.marker").exists()

    def test_lightweight_tag_at_expected_commit_uploads(self):
        proc, uploaded = self.run_attach("commit", SOURCE_SHA)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertTrue(uploaded)

    def test_annotated_tag_peeling_to_expected_commit_uploads(self):
        proc, uploaded = self.run_attach("tag", TAG_OBJECT_SHA, "commit", SOURCE_SHA)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertTrue(uploaded)

    def test_moved_tag_receives_no_upload(self):
        # Upload-only retry after the tag moves: the artifact still matches
        # the selected commit, but the tag no longer does.
        for ref_type, ref_sha, tag_type, tag_sha in (
            ("commit", MOVED_SHA, "", ""),
            ("tag", TAG_OBJECT_SHA, "commit", MOVED_SHA),
            ("tag", TAG_OBJECT_SHA, "tag", TAG_OBJECT_SHA),
        ):
            with self.subTest(ref_type=ref_type, tag_sha=tag_sha):
                proc, uploaded = self.run_attach(ref_type, ref_sha, tag_type, tag_sha)
                self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertFalse(uploaded)

    def test_source_mismatch_receives_no_upload(self):
        proc, uploaded = self.run_attach("commit", SOURCE_SHA, source_sha=MOVED_SHA)
        self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertFalse(uploaded)


if __name__ == "__main__":
    unittest.main()
