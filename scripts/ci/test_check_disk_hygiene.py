"""Hostile controls for the directly invoked, hyphenated disk-hygiene contract.

Mutate only temporary workflow fixtures, never the checkout. Real planner,
partition and final-check shells execute with the existing offline doubles.
"""

import importlib.util
import io
import re
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "disk_hygiene_contract", ROOT / "scripts/test-check-disk-hygiene.py",
)
DISK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DISK)


def replace_once(text, before, after):
    if text.count(before) != 1:
        raise AssertionError(f"mutation needs exactly one {before!r}")
    return text.replace(before, after, 1)


def edit_job(text, key, edit):
    match = re.search(rf"(?ms)^  {re.escape(key)}:\n.*?(?=^  [\w-]+:\s*$|\Z)", text)
    if match is None:
        raise AssertionError(f"missing fixture job {key}")
    return text[:match.start()] + edit(match[0]) + text[match.end():]


def step_block(job, name):
    match = re.search(rf"(?ms)^      - name: {re.escape(name)}\n.*?(?=^      - |\Z)", job)
    if match is None:
        raise AssertionError(f"missing fixture step {name}")
    return match[0]


class CheckDiskHygieneMutationTests(unittest.TestCase):
    def setUp(self):
        self.text = DISK.WORKFLOW.read_text()

    def run_contract(self, text, method=None):
        with tempfile.TemporaryDirectory(prefix="disk-hygiene-control-") as tmp:
            fixture = Path(tmp) / "check.yml"
            fixture.write_text(text)
            loader = unittest.TestLoader()
            suite = (unittest.TestSuite([DISK.CheckDiskHygieneTests(method)]) if method
                     else loader.loadTestsFromTestCase(DISK.CheckDiskHygieneTests))
            output = io.StringIO()
            with mock.patch.object(DISK, "WORKFLOW", fixture):
                result = unittest.TextTestRunner(stream=output, verbosity=2).run(suite)
            return result, output.getvalue()

    def assert_rejected(self, text, method):
        result, output = self.run_contract(text, method)
        self.assertEqual(result.testsRun, 1, output)
        self.assertEqual(result.errors, [], output)
        self.assertTrue(result.failures, "hostile workflow was accepted:\n" + output)

    def test_intact_contract_passes(self):
        result, output = self.run_contract(self.text)
        self.assertEqual(result.testsRun, len(unittest.TestLoader().getTestCaseNames(DISK.CheckDiskHygieneTests)))
        self.assertTrue(result.wasSuccessful(), output)

    def test_missing_disabled_or_nonfatal_worker_guard_is_rejected(self):
        for worker in DISK.PARTITION_LANES:
            for variant in ("missing", "disabled", "nonfatal"):
                with self.subTest(worker=worker, variant=variant):
                    def edit(job):
                        guard = step_block(job, DISK.GUARD)
                        if variant == "missing":
                            return replace_once(job, guard, "")
                        property_line = ("        if: false\n" if variant == "disabled"
                                         else "        continue-on-error: true\n")
                        return replace_once(job, guard, guard.replace("        shell: bash\n",
                                            property_line + "        shell: bash\n", 1))
                    self.assert_rejected(edit_job(self.text, worker, edit),
                                         "test_disk_guard_follows_prerequisites_in_every_lane")

    def test_worker_and_planner_dependency_edges_cannot_be_disconnected(self):
        for worker, lane in DISK.PARTITION_LANES.items():
            variants = (
                (lane, f"needs: [job-inputs, {worker}]", "needs: [job-inputs]"),
                (worker, "needs: [job-inputs, test-partition-plan]", "needs: [job-inputs]"),
            )
            for job, before, after in variants:
                with self.subTest(worker=worker, job=job):
                    bad = edit_job(self.text, job, lambda body: replace_once(body, before, after))
                    self.assert_rejected(bad, "test_guarded_workers_feed_strict_stable_final_checks")

    def test_partial_literal_matrix_cannot_replace_dynamic_discovery(self):
        for worker, lane in DISK.PARTITION_LANES.items():
            with self.subTest(worker=worker):
                before = f"matrix: ${{{{ fromJSON(needs.test-partition-plan.outputs.{lane}) }}}}"
                bad = edit_job(self.text, worker, lambda body: replace_once(
                    body, before, "matrix: {include: [{partition: integration-0}]}",
                ))
                self.assert_rejected(bad, "test_integration_coverage_not_narrowed")

    def test_script_mentions_and_arbitrary_cargo_text_are_not_execution(self):
        command = 'python3 scripts/ci/run-test-partition.py --lane "$CI_TEST_LANE" --partition "$CI_TEST_PARTITION"'
        variants = {
            "echo": f"run: echo '{command}'",
            "comment": f"run: 'true' # {command}",
            "env": f"run: |\n          export RUNNER_PROOF='{command}'\n          true",
            "cargo marker": 'run: echo "cargo test --workspace --test \'*\' --features two-bot-core/db --locked"',
        }
        for worker in DISK.PARTITION_LANES:
            for variant, replacement in variants.items():
                with self.subTest(worker=worker, variant=variant):
                    bad = edit_job(self.text, worker, lambda body: replace_once(
                        body, "run: " + command, replacement,
                    ))
                    self.assert_rejected(bad, "test_integration_coverage_not_narrowed")

    def test_post_suite_report_cannot_move_before_partition_execution(self):
        for worker in DISK.PARTITION_LANES:
            with self.subTest(worker=worker):
                def edit(job):
                    post = step_block(job, DISK.POST_REPORT)
                    return replace_once(replace_once(job, post, ""),
                                        "      - name: Run selected test partition\n",
                                        post + "\n      - name: Run selected test partition\n")
                self.assert_rejected(edit_job(self.text, worker, edit),
                                     "test_postgres_headroom_report_follows_verify")

    def test_reports_keep_service_ownership_queries_and_always_condition(self):
        for worker in DISK.PARTITION_LANES:
            for variant in ("service", "query", "condition", "connection"):
                with self.subTest(worker=worker, variant=variant):
                    def edit(job):
                        if variant == "service":
                            return re.sub(r"(?ms)^    services:\n.*?(?=^    steps:)", "", job, count=1)
                        post = step_block(job, DISK.POST_REPORT)
                        before, after = {
                            "query": ("datistemplate = false", "datistemplate = true"),
                            "condition": ("if: always()", "if: success()"),
                            "connection": ("@agent-testdb:5432/agent_test", "@other-service:5432/agent_test"),
                        }[variant]
                        return replace_once(job, post, replace_once(post, before, after))
                    method = ("test_postgres_headroom_report_follows_verify" if variant in ("service", "condition")
                              else "test_storage_reports_execute_exact_queries_and_fail_closed")
                    self.assert_rejected(edit_job(self.text, worker, edit), method)


if __name__ == "__main__":
    unittest.main()
