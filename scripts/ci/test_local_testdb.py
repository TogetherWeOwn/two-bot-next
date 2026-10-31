"""The local DB harness must keep mirroring check.yml's rust-tests job.

scripts/local_testdb.py hardcodes the job's test commands and environment so it
can run without a YAML parser. These offline checks fail when the workflow
changes and the harness does not, so local results keep meaning what CI means.
No Docker, database or network is used.
"""

import importlib.util
from pathlib import Path
import unittest

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
            if run.startswith(("cargo test", "python3 scripts/generate_command_reference.py")) \
                    and "property_" not in run:
                with self.subTest(step=step.get("name")):
                    self.assertIn(run, mirrored)

    def test_bootstrap_databases_match_prepare_step(self):
        prepare = next(step["run"] for step in self.job["steps"]
                       if step.get("id") == "prepare_test_databases")
        for name in self.harness.DATABASES:
            self.assertIn(f"createdb -h agent-testdb -U agent_test {name}\n", prepare)
        for key, value in self.harness.WAL:
            self.assertIn(f"ALTER SYSTEM SET {key} = '{value}'", prepare)


if __name__ == "__main__":
    unittest.main()
