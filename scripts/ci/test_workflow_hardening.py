"""Source and execution regressions for the ordinary workflow hardening."""

import hashlib
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]


def workflow(name):
    return yaml.load((ROOT / ".github/workflows" / name).read_text(), Loader=yaml.BaseLoader)


class WorkflowHardeningTests(unittest.TestCase):
    def test_selectors_treat_event_as_data_and_keep_fail_closed_defaults(self):
        jobs = workflow("check.yml")["jobs"]
        for job_id in ("job-inputs", "container-inputs"):
            step = next(s for s in jobs[job_id]["steps"] if s.get("id") == "select")
            self.assertEqual(step["env"]["EVENT_NAME"], "${{ github.event_name }}")
            self.assertNotIn("${{", step["run"])
            for event, succeeds, selected in (("pull_request", True, False),
                                               ("pull_request", False, True),
                                               ("workflow_dispatch", True, True),
                                               ("$(printf not-a-command)", True, True)):
                with self.subTest(job=job_id, event=event, succeeds=succeeds):
                    with tempfile.TemporaryDirectory() as tmp:
                        output = Path(tmp) / "output"
                        result = ("rust=false\\nworker=false\\nparity=false\\nsupply=false\\n"
                                  "docs=true\\nvoice=false\\n" if job_id == "job-inputs" else "false\\n")
                        shim = "python3() { printf '%b' '" + result + "'; return " + ("0" if succeeds else "1") + "; }\n"
                        proc = subprocess.run(
                            ["bash", "-e", "-o", "pipefail", "-c", shim + step["run"]],
                            env={"PATH": os.defpath, "EVENT_NAME": event, "BASE_SHA": "a" * 40,
                                 "HEAD_SHA": "b" * 40, "GITHUB_OUTPUT": str(output)},
                            capture_output=True, text=True, timeout=5,
                        )
                        self.assertEqual(proc.returncode, 0, proc.stderr)
                        values = dict(line.split("=", 1) for line in output.read_text().splitlines())
                        for key in (("rust", "worker", "parity", "supply", "voice")
                                    if job_id == "job-inputs" else ("build",)):
                            self.assertEqual(values[key], str(selected).lower())
                        if job_id == "job-inputs":
                            self.assertEqual(values["docs"], str(not selected).lower())
                        self.assertIn("(event=" + event + ")", proc.stdout)

    def test_supported_shell_transports_the_original_python_guards_unchanged(self):
        # Fingerprints cover the original reviewed programs, excluding trailing
        # blank lines only. Existing provenance/metadata behavior tests still run.
        expected = {
            "Resolve PR title/body": "e1c1be132514693ba3993562254b56ba02d6ee8932345ea9f350dbab3a3f7e1a",
            "Check title, body, branch and commits": "2ba8e0becf897f548c8ae501733369ca80cbc752409c20c7918387d203af2607",
        }
        steps = workflow("supply-chain.yml")["jobs"]["pr-lint"]["steps"]
        for name, digest in expected.items():
            with self.subTest(step=name):
                step = next(s for s in steps if s.get("name") == name)
                self.assertEqual(step["shell"], "bash")
                self.assertTrue(step["run"].startswith("python3 -I - <<'PY'\n"))
                # Override only the interpreter effect: execute the real Bash
                # transport and inspect its stdin, not shell-expanded source.
                shim = 'python3() { "$PYTHON" -c "import sys; sys.stdout.buffer.write(sys.stdin.buffer.read())"; }\n'
                proc = subprocess.run(
                    ["bash", "-e", "-o", "pipefail", "-c", shim + step["run"]],
                    env={"PATH": os.defpath, "PYTHON": sys.executable},
                    capture_output=True, timeout=5,
                )
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertEqual(hashlib.sha256(proc.stdout.rstrip(b"\n")).hexdigest(), digest)
                compile(proc.stdout, name, "exec")

    def test_python_guards_ignore_pr_root_modules_and_pythonpath(self):
        steps = workflow("supply-chain.yml")["jobs"]["pr-lint"]["steps"]
        for name in ("Resolve PR title/body", "Check title, body, branch and commits"):
            with self.subTest(step=name):
                step = next(s for s in steps if s.get("name") == name)
                argv = shlex.split(step["run"].splitlines()[0])
                self.assertEqual(argv[:3], ["python3", "-I", "-"])
                with tempfile.TemporaryDirectory() as tmp:
                    # Harmless import tripwire: it must never execute before
                    # the metadata guard establishes the trusted gate path.
                    Path(tmp, "json.py").write_text("raise AssertionError('untrusted root module')\n")
                    proc = subprocess.run(
                        [sys.executable, *argv[1:3]],
                        input="import json, sys; assert sys.flags.isolated; print(json.__name__)",
                        cwd=tmp, env={"PATH": os.defpath, "PYTHONPATH": tmp},
                        capture_output=True, text=True, timeout=5,
                    )
                    self.assertEqual(proc.returncode, 0, proc.stderr)
                    self.assertEqual(proc.stdout.strip(), "json")

    def test_new_explicit_names_preserve_the_previous_jobs_api_identity(self):
        for name, ids in {
            "nightly.yml": ("changes",),
            "pipeline-benchmark.yml": ("benchmark",),
            "release.yml": ("sbom-target", "verify-sbom", "attach-sbom"),
        }.items():
            for job_id in ids:
                with self.subTest(workflow=name, job=job_id):
                    self.assertEqual(workflow(name)["jobs"][job_id]["name"], job_id)

    def test_reported_permission_scopes_keep_their_grants_and_explanations(self):
        scopes = {
            "daily-digest.yml": {"rollback-proof-watch": {"actions": "read"}},
            "deploy-staging.yml": {"certify-promotion": {"actions": "read"}},
            "pr-size.yml": {"pr-size": {"issues": "write", "pull-requests": "write"},
                            "pr-size-weekly": {"issues": "read", "pull-requests": "read"}},
            "production-migrate.yml": {"apply": {"actions": "read"}},
            "staging-migrate.yml": {"apply": {"actions": "read"}},
            "release.yml": {"attach-sbom": {"contents": "write"}},
            "synthetic-user-flow.yml": {"staging": {"actions": "read"},
                                        "prod-passive": {"actions": "read"}},
            "test-guard.yml": {"test-guard": {"pull-requests": "read"}},
        }
        for name, jobs in scopes.items():
            doc = workflow(name)
            text = (ROOT / ".github/workflows" / name).read_text()
            for job_id, grants in jobs.items():
                # Use the parsed grant for semantics; examine the individual
                # source line separately so a YAML comment cannot change it.
                for scope, value in grants.items():
                    with self.subTest(workflow=name, job=job_id, scope=scope):
                        self.assertEqual(doc["jobs"][job_id]["permissions"][scope], value)
                        self.assertRegex(text.split("  " + job_id + ":\n", 1)[1],
                                         rf"(?m)^      {scope}: {value} # \S.+$")


if __name__ == "__main__":
    unittest.main()
