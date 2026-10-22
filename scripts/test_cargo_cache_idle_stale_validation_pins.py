#!/usr/bin/env python3
"""Builder-owned pins: non-boolean idle_stale refuses the whole retain run.

Companion to the protected acceptance class
``RetainIdleStaleAcceptanceTests`` in ``test_cargo_cache.py`` (Test Author
oracle, manifest sha256
``dac77a7f3cb2f83a149f44b21bd2c36fa34ebecef7da9dcecd25bdf330ce00ea``):
that class pins True/False/absent idle_stale with 6 tests and stays
byte-identical; this builder-owned module pins the 68081507 review
correction that rejects present non-boolean idle_stale during inventory
validation, so a control-plane producer drifting to string values such as
``"false"`` cannot silently lose the exit-75 whole-run Refusal.

Contract pinned here:

* Every present non-boolean idle_stale (strings, numbers, null, bytes,
  collections) on a nonterminal row AND on a terminal row raises
  ``Refusal('ambiguous slot attribution')`` before locks, process scanning,
  or any mutation.
* Both slots keep ``lease.json`` and ``target/debug/fixture`` after the
  refusal (whole-run preservation).
* Proven by mutant M1 (validation clause removed): this file goes red
  while the 80 protected + pre-existing tests stay green.

Offline only: synthetic pool + inventory fixtures, empty proc root, no
Cargo, network, services, or large fixtures.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_cargo_cache_idle_stale_validation_pins.py' -v
"""

import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest

import cargo_cache as cache


class IdleStaleValidationPins(unittest.TestCase):
    """Non-boolean idle_stale refuses the whole run with no mutation."""

    BAD_VALUES = ('false', 'true', 'True', '', 1, 0, 1.0, 0.0, None,
                  b'false', [], {}, (), 2, -1)
    STATUSES = ('in_review', 'done')

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get('PAPERCLIP_RUN_SCRATCH_DIR'))
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.pool = self.root / 'pool'
        self.pool.mkdir()
        self.policy = {'version': 1, 'slots': 2, 'slot_budget_bytes': 256 * 1024,
                       'min_available_bytes': 1, 'hard_limit_bytes': 1024 * 1024,
                       'quota_receipt': 'synthetic fixture; not a real quota receipt',
                       'scratch_coverage_receipt': 'synthetic fixture; not host coverage'}
        (self.pool / 'policy.json').write_text(json.dumps(self.policy))
        for number in range(2):
            slot = self.pool / f'slot-{number}'
            slot.mkdir()
            target = slot / 'target'
            target.mkdir()
            (target / 'debug').mkdir()
            (target / 'debug' / 'fixture').write_bytes(b'x' * 4096)
            (target / '.rustc_info.json').write_text('{}')
            scratch = slot / 'scratch'
            scratch.mkdir()
            (scratch / 'tmp-out').write_bytes(b'x' * 4096)
            (slot / 'lock').touch()
            (slot / 'lease.json').write_text(json.dumps(
                {'workspace': str(self.root), 'wrapper_pid': 99999999,
                 'started_at': 0, 'cargo_pgid': 99999998}))
        self.proc = self.root / 'proc'
        self.proc.mkdir()
        self.now = time.time()
        self.inventory = {'version': 1, 'complete': True, 'process_scope': 'host',
                          'captured_at_unix': self.now,
                          'slots': [self.row(f'slot-{n}') for n in range(2)]}

    def row(self, name):
        return {'path': str(self.pool / name), 'issue_id': 'fixture-terminal',
                'status': 'done', 'live_run': False, 'referenced': False,
                'target_provenance': 'build_output_only'}

    def retain(self, **kwargs):
        kwargs.setdefault('proc_root', self.proc)
        kwargs.setdefault('now', self.now)
        return cache.shared_pool_retain(self.pool, self.inventory, **kwargs)

    def assert_slots_intact(self):
        for number in range(2):
            slot = self.pool / f'slot-{number}'
            self.assertTrue((slot / 'lease.json').exists(),
                            f'slot-{number} lease.json mutated')
            self.assertTrue((slot / 'target' / 'debug' / 'fixture').exists(),
                            f'slot-{number} target fixture mutated')

    def test_non_boolean_idle_stale_refuses_whole_run_without_mutation(self):
        # Pin for the 68081507 correction: validation runs before locks and
        # the process scan, so every present non-boolean idle_stale refuses
        # the whole run with no mutation, on terminal rows as well as
        # nonterminal rows. Removing the validation clause must turn this
        # test red while the 80 tests in test_cargo_cache.py stay green.
        for status in self.STATUSES:
            for bad in self.BAD_VALUES:
                with self.subTest(status=status, bad=repr(bad)):
                    row = self.inventory['slots'][0]
                    row['status'] = status
                    row['idle_stale'] = bad
                    try:
                        with self.assertRaisesRegex(cache.Refusal,
                                                    'ambiguous slot attribution'):
                            self.retain()
                    finally:
                        row.pop('idle_stale', None)
                        row['status'] = 'done'
                    self.assert_slots_intact()


if __name__ == '__main__':
    unittest.main()
