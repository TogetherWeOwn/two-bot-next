#!/usr/bin/env python3
"""Actions-only baseline counterexample and five corrected sampled-stop runs."""

import ast
import linecache
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_cargo_cache as fixtures


# Main immediately before the oracle repair; retain the exact old assertions.
BASELINE = 'a8ca2c02cfcced654a680853b93655f898bbc51f'
BASE_CASES = (
    ('test_guard_stops_growth_and_retains_lease', 32 * 1024),
    ('test_scratch_growth_is_counted_and_stopped', 40 * 1024),
)
CORRECTED = (
    'test_guard_stops_growth_and_retains_lease',
    'test_target_growth_is_stopped_with_delayed_sampler',
    'test_scratch_growth_is_counted_and_stopped',
    'test_scratch_growth_is_stopped_with_delayed_sampler',
)


def baseline_case(source, name):
    tree = ast.parse(source)
    cls = next(node for node in tree.body
               if isinstance(node, ast.ClassDef) and node.name == 'CacheTests')
    method = next(node for node in cls.body
                  if isinstance(node, ast.FunctionDef) and node.name == name)
    namespace = dict(vars(fixtures))
    filename = f'{BASELINE}:scripts/test_cargo_cache.py'
    linecache.cache[filename] = (len(source), None, source.splitlines(True), filename)
    # Load only the two original methods, not historical setup or cache code.
    exec(compile(ast.Module(body=[method], type_ignores=[]), filename, 'exec'), namespace)
    return type('BaselineCeiling', (fixtures.CacheTests,), {name: namespace[name]})(name)


def prove_baseline(source, name, ceiling):
    case = baseline_case(source, name)
    real_sleep = time.sleep
    observed = []

    def delayed_sample(seconds):
        if seconds != .01 or observed:
            return real_sleep(seconds)
        # Model a sampler descheduled while the original 4 KiB writer keeps
        # running. Resume only after its file exceeds the unchanged ceiling.
        # The deadline fails closed if the writer never reaches that point.
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            for growing in case.pool.glob('slot-0/*/*/growing'):
                allocated = growing.stat().st_blocks * 512
                if allocated > ceiling:
                    observed.append(allocated)
                    return
            real_sleep(.01)
        raise RuntimeError('baseline writer did not exceed the original ceiling')

    result = unittest.TestResult()
    with patch.object(fixtures.cache.time, 'sleep', delayed_sample):
        case.run(result)
    # Only the original ceiling failure is evidence. Setup/refusal/cleanup
    # errors and unrelated assertions must not become an expected red result.
    if (result.testsRun != 1 or result.errors or len(result.failures) != 1
            or not observed):
        raise RuntimeError(f'invalid baseline result: {result.errors!r} {result.failures!r}')
    traceback = result.failures[0][1]
    if 'self.assertLessEqual(' not in traceback or f'not less than or equal to {ceiling}' not in traceback:
        raise RuntimeError(f'baseline failed outside the original ceiling: {traceback}')
    print(f'BASELINE {BASELINE} {name}: original ceiling {ceiling} exceeded '
          f'at {observed[0]} allocated bytes; expected assertion failure\n{traceback}', flush=True)


def main():
    if os.environ.get('GITHUB_ACTIONS') != 'true':
        raise SystemExit('run this process-group proof only on GitHub Actions')
    source = subprocess.run(
        ['git', 'show', f'{BASELINE}:scripts/test_cargo_cache.py'],
        check=True, capture_output=True, text=True, timeout=10).stdout
    head = subprocess.run(['git', 'rev-parse', 'HEAD'], check=True,
                          capture_output=True, text=True, timeout=10).stdout.strip()
    print(f'Sampled-stop proof checkout {head}; baseline {BASELINE}', flush=True)
    for name, ceiling in BASE_CASES:
        prove_baseline(source, name, ceiling)
    for repeat in range(1, 6):
        print(f'CORRECTED {head} repetition {repeat}/5', flush=True)
        suite = unittest.TestSuite(fixtures.CacheTests(name) for name in CORRECTED)
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        if not result.wasSuccessful() or result.testsRun != len(CORRECTED):
            raise SystemExit(1)
    print(f'PASS {head}: both baseline ceiling counterexamples; corrected 5/5 (20 cases)', flush=True)


if __name__ == '__main__':
    main()
