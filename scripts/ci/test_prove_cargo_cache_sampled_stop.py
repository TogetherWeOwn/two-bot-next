#!/usr/bin/env python3
"""Source/result handling only: no process-group experiment on the host."""

import contextlib
import io
import linecache
import os
import unittest
from unittest.mock import MagicMock, patch

import prove_cargo_cache_sampled_stop as proof


class SampledStopProofTests(unittest.TestCase):
    def test_baseline_extracts_only_requested_method_with_original_trace(self):
        source = ('raise RuntimeError("top-level code must not run")\n'
                  'class CacheTests:\n'
                  ' def test_ceiling(self):\n'
                  '  self.assertLessEqual(65536, 32768)\n')
        case = proof.baseline_case(source, 'test_ceiling')
        result = unittest.TestResult()
        case.run(result)
        self.assertEqual(result.testsRun, 1)
        self.assertEqual(result.errors, [])
        self.assertEqual(len(result.failures), 1)
        self.assertIn('self.assertLessEqual(65536, 32768)', result.failures[0][1])
        self.assertIn('not less than or equal to 32768', result.failures[0][1])
        linecache.cache.pop(f'{proof.BASELINE}:scripts/test_cargo_cache.py', None)

    def test_refuses_execution_outside_actions(self):
        with patch.dict(os.environ, {'GITHUB_ACTIONS': 'false'}), \
                patch.object(proof.subprocess, 'run') as run:
            with self.assertRaisesRegex(SystemExit, 'only on GitHub Actions'):
                proof.main()
        run.assert_not_called()

    def test_baseline_rejects_false_green_and_unrelated_errors(self):
        for errors, failures in (([], []), (['setup failed'], []),
                                 ([], [('case', 'unrelated assertion')])):
            with self.subTest(errors=errors, failures=failures):
                case = MagicMock()
                case.pool.glob.return_value = [MagicMock()]
                case.pool.glob.return_value[0].stat.return_value.st_blocks = 128
                (case.pool / 'slot-0' / 'lease.json').read_text.return_value = '{"cargo_pgid": 123}'

                def run(result):
                    proof.fixtures.cache.usage(case.pool / 'slot-0')
                    result.testsRun = 1
                    result.errors = errors
                    result.failures = failures

                case.run.side_effect = run
                with patch.object(proof, 'baseline_case', return_value=case), \
                        patch.object(proof.fixtures.cache, 'usage', return_value=65536):
                    with self.assertRaisesRegex(RuntimeError, 'baseline'):
                        proof.prove_baseline('', 'test_ceiling', 32768)

    def test_delay_precedes_first_post_spawn_sample_without_changing_admission(self):
        case = MagicMock()
        slot = case.pool / 'slot-0'
        lease = slot / 'lease.json'
        lease.exists.side_effect = [False, True]
        lease.read_text.return_value = '{"cargo_pgid": 123}'
        case.pool.glob.return_value = [MagicMock()]
        case.pool.glob.return_value[0].stat.return_value.st_blocks = 128

        def run(result):
            # No sleep call between admission and the first budget sample.
            self.assertEqual(proof.fixtures.cache.usage(slot), 24576)
            case.pool.glob.assert_not_called()
            self.assertEqual(proof.fixtures.cache.usage(slot), 24576)
            result.testsRun = 1
            result.failures = [('case', 'self.assertLessEqual(\n65536 not less than or equal to 32768')]

        case.run.side_effect = run
        with patch.object(proof, 'baseline_case', return_value=case), \
                patch.object(proof.fixtures.cache, 'usage', return_value=24576) as usage, \
                patch.object(proof.time, 'sleep', side_effect=AssertionError('unexpected sleep')), \
                contextlib.redirect_stdout(io.StringIO()):
            proof.prove_baseline('', 'test_ceiling', 32768)
        self.assertEqual(usage.call_count, 2)
        case.pool.glob.assert_called_once_with('slot-0/*/*/growing')

    def test_main_runs_both_baselines_and_five_complete_corrected_suites(self):
        runner = MagicMock()
        runner.run.return_value.testsRun = 4
        runner.run.return_value.wasSuccessful.return_value = True
        with patch.dict(os.environ, {'GITHUB_ACTIONS': 'true'}), \
                patch.object(proof.subprocess, 'run') as git, \
                patch.object(proof, 'prove_baseline') as baseline, \
                patch.object(proof.unittest, 'TextTestRunner', return_value=runner), \
                contextlib.redirect_stdout(io.StringIO()):
            git.return_value.stdout = 'synthetic source or sha'
            proof.main()
        self.assertEqual(baseline.call_count, 2)
        self.assertEqual(runner.run.call_count, 5)
        for call in runner.run.call_args_list:
            suite = call.args[0]
            self.assertEqual([case._testMethodName for case in suite], list(proof.CORRECTED))
        self.assertEqual(git.call_args_list[0].args[0],
                         ['git', 'show', f'{proof.BASELINE}:scripts/test_cargo_cache.py'])

    def test_main_rejects_incomplete_corrected_suite(self):
        runner = MagicMock()
        runner.run.return_value.testsRun = 3
        runner.run.return_value.wasSuccessful.return_value = True
        with patch.dict(os.environ, {'GITHUB_ACTIONS': 'true'}), \
                patch.object(proof.subprocess, 'run'), \
                patch.object(proof, 'prove_baseline'), \
                patch.object(proof.unittest, 'TextTestRunner', return_value=runner), \
                contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit):
                proof.main()
        self.assertEqual(runner.run.call_count, 1)


if __name__ == '__main__':
    unittest.main()
