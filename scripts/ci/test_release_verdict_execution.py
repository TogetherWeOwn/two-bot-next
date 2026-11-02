"""Execute the release verdict offline; never publish or dispatch a workflow."""

import os
from pathlib import Path
import subprocess
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
JOB_RESULTS = {
    "sbom-target": "SBOM_TARGET_RESULT",
    "release-sbom": "RELEASE_SBOM_RESULT",
    "verify-sbom": "VERIFY_SBOM_RESULT",
    "attach-sbom": "ATTACH_SBOM_RESULT",
}


class ReleaseVerdictExecutionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        workflow = yaml.safe_load(
            (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        )
        cls.shell = workflow["jobs"]["verdict"]["steps"][0]["run"]

    def run_verdict(self, inputs, results):
        # Do not inherit dispatch inputs, job results or credentials from the host.
        env = {
            "PATH": os.defpath,
            "DRY_RUN": "false",
            "RELEASE_TAG": "",
            "DISPATCH_REF": "refs/heads/main",
            **inputs,
            **dict(zip(JOB_RESULTS.values(), results)),
        }
        return subprocess.run(
            ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", self.shell],
            cwd=ROOT, env=env, capture_output=True, text=True, timeout=10,
        )

    def test_empty_dispatch_fails_and_names_every_skipped_job(self):
        # Inverting the dry-run comparison must not turn an empty dispatch green.
        for dry_run in ("false", ""):
            with self.subTest(dry_run=dry_run):
                proc = self.run_verdict({"DRY_RUN": dry_run}, ("skipped",) * 4)
                output = proc.stdout + proc.stderr
                self.assertEqual(proc.returncode, 1, output)
                self.assertIn("::error::empty dispatch:", output)
                self.assertIn("dry_run is false", output)
                self.assertIn("release_tag is empty", output)
                for job in JOB_RESULTS:
                    self.assertIn(job, output)
                self.assertNotIn("release verdict passed", output)

    def test_publication_from_a_non_main_ref_fails_closed(self):
        # The target guard skips the whole chain off main; the verdict must
        # name the ref instead of reading the designed skips as green.
        for ref in ("refs/heads/feat/example", "refs/tags/v2026.1101.0", ""):
            with self.subTest(ref=ref):
                proc = self.run_verdict(
                    {"RELEASE_TAG": "v2026.1101.0", "DISPATCH_REF": ref},
                    ("skipped",) * 4)
                output = proc.stdout + proc.stderr
                self.assertEqual(proc.returncode, 1, output)
                self.assertIn("::error::wrong ref:", output)
                self.assertIn("refs/heads/main", output)
                self.assertNotIn("release verdict passed", output)

    def test_selected_dispatches_keep_designed_skips_green(self):
        cases = (
            ("digest", {"RELEASE_TAG": "v2026.10.21"},
             ("success", "success", "success", "success")),
            ("dry-run", {"DRY_RUN": "true"},
             ("success", "success", "skipped", "skipped")),
            ("dry-run-branch", {"DRY_RUN": "true", "DISPATCH_REF": "refs/heads/feat/example"},
             ("success", "success", "skipped", "skipped")),
            ("repair", {"RELEASE_TAG": "v1.2.3"},
             ("success", "success", "success", "success")),
        )
        for label, inputs, results in cases:
            with self.subTest(label=label):
                proc = self.run_verdict(inputs, results)
                output = proc.stdout + proc.stderr
                self.assertEqual(proc.returncode, 0, output)
                self.assertIn("release verdict passed", output)
                self.assertNotIn("::error::", output)

    def test_each_failed_or_cancelled_dependency_fails_the_verdict(self):
        # Dropping either terminal failure state, or reading the wrong result
        # variable, must not hide a dependency failure behind a green verdict.
        for index, job in enumerate(JOB_RESULTS):
            for result in ("failure", "cancelled"):
                with self.subTest(job=job, result=result):
                    results = ["success"] * 4
                    results[index] = result
                    proc = self.run_verdict({"RELEASE_TAG": "v2026.10.21"}, results)
                    output = proc.stdout + proc.stderr
                    self.assertEqual(proc.returncode, 1, output)
                    self.assertIn(f"::error::{job}: result='{result}'", output)
                    self.assertNotIn("release verdict passed", output)


if __name__ == "__main__":
    unittest.main()
