"""The local DB harness must keep mirroring check.yml's rust-tests job.

scripts/local_testdb.py hardcodes the job's test commands and environment so it
can run without a YAML parser. These offline checks fail when the workflow
changes and the harness does not, so local results keep meaning what CI means.
No Docker, database or network is used.
"""

import importlib.util
import re
from pathlib import Path
import subprocess
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
JOB = "rust-tests"


def load_harness():
    spec = importlib.util.spec_from_file_location("local_testdb", ROOT / "scripts/local_testdb.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def rust_tests_job():
    import yaml

    workflow = yaml.load((ROOT / ".github/workflows/check.yml").read_text(), Loader=yaml.BaseLoader)
    return workflow["jobs"][JOB]


def words(text):
    return " ".join(text.replace("\\\n", " ").split())


class LocalTestDbParity(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.harness = load_harness()
        cls.job = rust_tests_job()

    def test_images_are_the_jobs_digest_pins(self):
        postgres, rust = self.harness.images()
        self.assertEqual(postgres, self.job["services"]["agent-testdb"]["image"])
        self.assertEqual(rust, self.job["container"]["image"])

    def test_job_env_matches(self):
        expected = {key: value for key, value in self.job["env"].items()
                    if key != "CARGO_TERM_COLOR"}
        self.assertEqual(self.harness.JOB_ENV, expected)

    def test_every_harness_step_is_a_job_step_with_the_same_env(self):
        steps = {words(step.get("run", "")): step.get("env", {}) for step in self.job["steps"]}
        for name, env, command in self.harness.STEPS:
            with self.subTest(step=name):
                self.assertIn(words(command), steps)
                ci_env = {key: value for key, value in steps[words(command)].items()
                          if key != "PAPERCLIP_RUN_SCRATCH_DIR"}
                self.assertEqual(env, ci_env)

    def test_every_job_test_step_is_mirrored(self):
        mirrored = {words(command) for _, _, command in self.harness.STEPS}
        for step in self.job["steps"]:
            run = words(step.get("run", ""))
            if re.search(r"(^| )(cargo (test|run) |python3 scripts/generate_command_reference\.py)", run):
                with self.subTest(step=step.get("name")):
                    self.assertIn(run, mirrored)

    def test_steps_run_under_the_jobs_default_shell(self):
        # A step with no `shell:` runs as `bash -e {0}` on GitHub.
        for step in self.job["steps"]:
            if re.search(r"(^| )cargo (test|run) ", words(step.get("run", ""))):
                with self.subTest(step=step.get("name")):
                    self.assertNotIn("shell", step)
        self.assertEqual(self.harness.SHELL, ("bash", "-e", "-c"))

    def test_failed_cargo_fails_the_property_budget_step(self):
        command = dict((name, command) for name, _, command in self.harness.STEPS)["property budget"]
        stubbed = "cargo() { return 101; }\n" + command
        result = subprocess.run([*self.harness.SHELL, stubbed], capture_output=True)
        self.assertEqual(result.returncode, 101)

    def test_bootstrap_databases_match_prepare_step(self):
        prepare = next(step["run"] for step in self.job["steps"]
                       if step.get("id") == "prepare_test_databases")
        for name in self.harness.DATABASES:
            self.assertIn(f"createdb -h agent-testdb -U agent_test {name}\n", prepare)
        for key, value in self.harness.WAL:
            self.assertIn(f"ALTER SYSTEM SET {key} = '{value}'", prepare)


class RunnerProvisioning(unittest.TestCase):
    def test_failed_provisioning_removes_the_runner(self):
        harness = load_harness()
        calls = []

        def docker(*args, check=True, capture=False):
            calls.append(args)
            return subprocess.CompletedProcess(args, 0, "", "")

        failure = subprocess.CalledProcessError(100, ["apt-get"])
        with mock.patch.object(harness, "docker", docker), \
                mock.patch.object(harness, "running", return_value=False), \
                mock.patch.object(harness, "exists", return_value=True), \
                mock.patch.object(harness, "runner_exec", side_effect=failure):
            with self.assertRaises(subprocess.CalledProcessError):
                harness.up()
        self.assertEqual(calls[-1], ("rm", "-f", harness.RUNNER))


if __name__ == "__main__":
    unittest.main()
