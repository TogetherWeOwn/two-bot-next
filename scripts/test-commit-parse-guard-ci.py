"""Keep parser installation and execution separately visible in CI metadata.

Offline and stdlib-only; step conclusions survive even when runner logs do not.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
INSTALL = 'npm install --prefix "$RUNNER_TEMP/release-fixture" --ignore-scripts --no-audit --no-fund release-please@17.6.0'
GUARD = 'NODE_PATH="$RUNNER_TEMP/release-fixture/node_modules" node ../scripts/test-commit-parse-guard.cjs'
SELECTED = "        if: needs.job-inputs.outputs.worker != 'false'"


def worker_steps(text):
    worker = re.search(r"(?ms)^  worker:\n(.*?)(?=^  [A-Za-z0-9_-]+:|\Z)", text)
    if worker is None:
        raise ValueError("worker job missing")
    return re.split(r"(?m)(?=^      - )", worker.group(1))[1:]


class CommitParserWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.text = (ROOT / ".github/workflows/check.yml").read_text()
        self.steps = worker_steps(self.text)

    def test_install_and_guard_are_distinct_ordered_steps(self):
        installs = [i for i, step in enumerate(self.steps) if INSTALL in step]
        guards = [i for i, step in enumerate(self.steps) if GUARD in step]
        self.assertEqual(len(installs), 1)
        self.assertEqual(len(guards), 1)
        self.assertEqual(guards[0], installs[0] + 1,
                         "install and guard need separate, consecutive step conclusions")
        for index, command in [(installs[0], INSTALL), (guards[0], GUARD)]:
            step = self.steps[index]
            self.assertIn(SELECTED, step.splitlines())
            self.assertRegex(step, r"^      - name: .+")
            self.assertNotIn("continue-on-error:", step)
            self.assertNotIn("timeout-minutes:", step)
            commands = [line.strip() for line in step.splitlines()
                        if line.startswith("          ") and not line.lstrip().startswith("#")]
            inline = re.search(r"(?m)^        run: (?!\|)(.+)$", step)
            self.assertEqual(commands if inline is None else [inline.group(1)], [command])

    def test_ci_runs_this_offline_suite(self):
        invocation = "python3 ../scripts/test-commit-parse-guard-ci.py"
        steps = [step for step in self.steps if invocation in step]
        self.assertEqual(len(steps), 1)
        self.assertIn(SELECTED, steps[0].splitlines())
        self.assertNotIn("continue-on-error:", steps[0])


if __name__ == "__main__":
    unittest.main()
