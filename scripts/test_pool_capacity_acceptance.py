#!/usr/bin/env python3
"""Pool capacity-contract acceptance: provisioning warning + floor/lease pins.

A-tests define the startup warning contract (run_cargo, after load_policy,
before the floor check): if floor + slots*budget > fs_size or hard_limit >
fs_size (statvfs on the pool path), print a structured stderr JSON finding
and continue unchanged (warning, never refusal).

Positive A-cases (A1, A4-A8) are RED on current code (no warning exists yet)
and go green when the Builder implements the finding. A2/A3/A9 are no-finding
negative controls: green before and after by design. A10 pins frsize scaling:
24 GiB stays silent, 20 GiB warns with the exact byte size.

B-tests pin current behavior and must stay green before and after.

Finding shape (adopted exactly as recommended, no deviation):
  {"finding": "pool_provisioning_mismatch",
   "floor_plus_budgets_bytes": int, "hard_limit_bytes": int,
   "filesystem_size_bytes": int, "slots": int,
   "slot_budget_bytes": int, "min_available_bytes": int}

Offline only: tmp fixture pools, `true` stub or tiny Python stubs, mocked
os.statvfs / cache.available. No network, no live pool, no compiling Cargo.
"""

import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import cargo_cache as cache


GIB = 1024 ** 3

HOST_SLOTS = 2
HOST_BUDGET = 6442450944  # 6 GiB
HOST_FLOOR = 10737418240  # 10 GiB (min_available_bytes)
HOST_HARD = 17179869184  # 16 GiB
HOST_SIZE = 16729894912  # host loop fs size (~15.58 GiB)
HOST_AVAIL = 10735038464  # host avail < floor (floor symptom)
HOST_FLOOR_PLUS = HOST_FLOOR + HOST_SLOTS * HOST_BUDGET  # 22 GiB


class FakeStatvfs:
    """Minimal statvfs double: exact byte sizes via frsize=1 by default."""

    def __init__(self, size_bytes, avail_bytes, frsize=1):
        self.f_frsize = frsize
        self.f_blocks = size_bytes // frsize
        self.f_bavail = avail_bytes // frsize
        self.f_bfree = self.f_bavail


def provisioning_findings(stderr_text):
    """All pool_provisioning_mismatch JSON objects on stderr."""
    out = []
    for line in stderr_text.strip().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if isinstance(obj, dict) and obj.get('finding') == 'pool_provisioning_mismatch':
            out.append(obj)
    return out


def stderr_json_findings(stderr_text):
    """All JSON findings on stderr, in order (for precedence checks)."""
    out = []
    for line in stderr_text.strip().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if isinstance(obj, dict) and 'finding' in obj:
            out.append(obj)
    return out


class PoolFixture(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get('PAPERCLIP_RUN_SCRATCH_DIR'))
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.pool = self.root / 'pool'
        self.pool.mkdir()
        self.true_bin = shutil.which('true') or '/bin/true'

    def save_policy(self, policy):
        self.policy = policy
        (self.pool / 'policy.json').write_text(json.dumps(policy))

    def make_slots(self, count=2):
        for number in range(count):
            slot = self.pool / f'slot-{number}'
            slot.mkdir(exist_ok=True)
            (slot / 'target').mkdir(exist_ok=True)
            (slot / 'scratch').mkdir(exist_ok=True)
            (slot / 'lock').touch(exist_ok=True)

    def host_policy(self):
        return {'version': 1, 'slots': HOST_SLOTS,
                'slot_budget_bytes': HOST_BUDGET,
                'min_available_bytes': HOST_FLOOR,
                'hard_limit_bytes': HOST_HARD,
                'quota_receipt': 'synthetic fixture; not a real quota receipt',
                'scratch_coverage_receipt': 'synthetic fixture; not host coverage'}

    def small_policy(self, floor=4096, budget=256 * 1024, hard=1024 * 1024):
        return {'version': 1, 'slots': 2, 'slot_budget_bytes': budget,
                'min_available_bytes': floor, 'hard_limit_bytes': hard,
                'quota_receipt': 'synthetic fixture; not a real quota receipt',
                'scratch_coverage_receipt': 'synthetic fixture; not host coverage'}

    def dead_pgid(self):
        terminated = subprocess.Popen([self.true_bin])
        terminated.wait()
        self.assertFalse(cache.group_alive(terminated.pid))
        return terminated.pid

    def leases(self):
        return list(self.pool.glob('slot-*/lease.json'))


class WarningTests(PoolFixture):
    """A-suite: the provisioning warning contract."""

    def test_a1_host_mismatch_warns_and_still_admits(self):
        self.save_policy(self.host_policy())
        self.make_slots()
        fake = FakeStatvfs(HOST_SIZE, HOST_FLOOR)
        err = io.StringIO()
        with patch.object(os, 'statvfs', return_value=fake):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        findings = provisioning_findings(err.getvalue())
        self.assertEqual(len(findings), 1,
                         f'expected one provisioning finding, stderr={err.getvalue()!r}')
        found = findings[0]
        self.assertEqual(found['floor_plus_budgets_bytes'], HOST_FLOOR_PLUS)
        self.assertEqual(found['hard_limit_bytes'], HOST_HARD)
        self.assertEqual(found['filesystem_size_bytes'], HOST_SIZE)
        self.assertEqual(found['slots'], HOST_SLOTS)
        self.assertEqual(found['slot_budget_bytes'], HOST_BUDGET)
        self.assertEqual(found['min_available_bytes'], HOST_FLOOR)
        self.assertEqual(self.leases(), [])

    def test_a2_huge_filesystem_has_no_finding(self):
        # Negative control: 500 GiB fits floor+budgets (22 GiB) and hard (16 GiB).
        self.save_policy(self.host_policy())
        self.make_slots()
        fake = FakeStatvfs(500 * GIB, HOST_FLOOR)
        err = io.StringIO()
        with patch.object(os, 'statvfs', return_value=fake):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        self.assertEqual(provisioning_findings(err.getvalue()), [],
                         f'unexpected finding, stderr={err.getvalue()!r}')
        self.assertEqual(self.leases(), [])

    def test_a3_roomy_filesystem_has_no_finding(self):
        # Negative control: 24 GiB fits floor+budgets (22 GiB) and hard (16 GiB).
        self.save_policy(self.host_policy())
        self.make_slots()
        fake = FakeStatvfs(24 * GIB, HOST_FLOOR)
        err = io.StringIO()
        with patch.object(os, 'statvfs', return_value=fake):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        self.assertEqual(provisioning_findings(err.getvalue()), [],
                         f'unexpected finding, stderr={err.getvalue()!r}')
        self.assertEqual(self.leases(), [])

    def test_a4_hard_only_over_emits_finding(self):
        # floor 1 + 2x1 budgets = 3 <= size 10, but hard 100 > 10: hard-only.
        # 1-byte budgets mean empty fixture slots are over budget, so admission
        # ends 'no idle' after the warning (warning precedes acquire, like A6).
        policy = {'version': 1, 'slots': 2, 'slot_budget_bytes': 1,
                  'min_available_bytes': 1, 'hard_limit_bytes': 100,
                  'quota_receipt': 'synthetic fixture; not a real quota receipt',
                  'scratch_coverage_receipt': 'synthetic fixture; not host coverage'}
        self.save_policy(policy)
        self.make_slots()
        fake = FakeStatvfs(10, 10)
        err = io.StringIO()
        with patch.object(os, 'statvfs', return_value=fake):
            with contextlib.redirect_stderr(err):
                with self.assertRaisesRegex(cache.Refusal, 'no idle'):
                    cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        findings = provisioning_findings(err.getvalue())
        self.assertEqual(len(findings), 1,
                         f'expected hard-only finding, stderr={err.getvalue()!r}')
        found = findings[0]
        self.assertEqual(found['floor_plus_budgets_bytes'], 3)
        self.assertEqual(found['hard_limit_bytes'], 100)
        self.assertEqual(found['filesystem_size_bytes'], 10)
        self.assertEqual(found['slots'], 2)
        self.assertEqual(found['slot_budget_bytes'], 1)
        self.assertEqual(found['min_available_bytes'], 1)
        self.assertEqual(self.leases(), [])

    def test_a5_finding_precedes_floor_refusal(self):
        self.save_policy(self.host_policy())
        self.make_slots()
        fake = FakeStatvfs(HOST_SIZE, HOST_AVAIL)
        err = io.StringIO()
        with patch.object(os, 'statvfs', return_value=fake):
            with contextlib.redirect_stderr(err):
                with self.assertRaisesRegex(cache.Refusal, 'insufficient'):
                    cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        # Presence proves precedence: the floor raises, so a finding printed
        # after the floor check could never appear.
        findings = provisioning_findings(err.getvalue())
        self.assertEqual(len(findings), 1,
                         f'finding must precede floor refusal, stderr={err.getvalue()!r}')
        self.assertEqual(self.leases(), [])

    def test_a6_finding_precedes_no_idle_refusal(self):
        self.save_policy(self.host_policy())
        self.make_slots()
        for slot in self.pool.glob('slot-*'):
            (slot / 'lease.json').write_text(json.dumps({'wrapper_pid': 99999999}))
        fake = FakeStatvfs(HOST_SIZE, HOST_FLOOR)
        err = io.StringIO()
        with patch.object(os, 'statvfs', return_value=fake):
            with contextlib.redirect_stderr(err):
                with self.assertRaisesRegex(cache.Refusal, 'no idle'):
                    cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        findings = provisioning_findings(err.getvalue())
        self.assertEqual(len(findings), 1,
                         f'warning needs no lock/lease, stderr={err.getvalue()!r}')
        self.assertEqual(len(self.leases()), 2)

    def test_a7_finding_and_stale_recovery_both_proceed(self):
        self.save_policy(self.host_policy())
        self.make_slots()
        slot = self.pool / 'slot-0'
        lease = {'workspace': str(self.root), 'wrapper_pid': 99999999,
                 'started_at': 0, 'cargo_pgid': self.dead_pgid()}
        (slot / 'lease.json').write_text(json.dumps(lease))
        fake = FakeStatvfs(HOST_SIZE, HOST_FLOOR)
        err = io.StringIO()
        with patch.object(os, 'statvfs', return_value=fake):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        ordered = stderr_json_findings(err.getvalue())
        kinds = [row['finding'] for row in ordered]
        self.assertIn('pool_provisioning_mismatch', kinds,
                      f'missing warning, stderr={err.getvalue()!r}')
        self.assertIn('stale_lease_recovered', kinds,
                      f'warning must never block recovery, stderr={err.getvalue()!r}')
        self.assertLess(kinds.index('pool_provisioning_mismatch'),
                        kinds.index('stale_lease_recovered'),
                        'warning precedes acquire/recovery')

    def test_a8_floor_branch_only_warns_and_admits(self):
        # Floor-plus-budgets 22 GiB > size 20 GiB, but hard 16 GiB < 20 GiB:
        # the finding can only come from the floor term (pins the branch a
        # mutant would drop from the condition).
        self.save_policy(self.host_policy())
        self.make_slots()
        fake = FakeStatvfs(20 * GIB, HOST_FLOOR)
        err = io.StringIO()
        with patch.object(os, 'statvfs', return_value=fake):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        findings = provisioning_findings(err.getvalue())
        self.assertEqual(len(findings), 1,
                         f'expected one floor-branch finding, stderr={err.getvalue()!r}')
        found = findings[0]
        self.assertLess(found['hard_limit_bytes'], found['filesystem_size_bytes'],
                        'hard term must be false so only the floor term can fire')
        self.assertEqual(found['floor_plus_budgets_bytes'], HOST_FLOOR_PLUS)
        self.assertEqual(found['filesystem_size_bytes'], 20 * GIB)
        self.assertEqual(self.leases(), [])

    def test_a9_exact_fit_boundary_has_no_finding(self):
        # size == floor_plus_budgets (22 GiB): strict greater-than means no
        # finding (pins the boundary a >= mutant would cross).
        self.save_policy(self.host_policy())
        self.make_slots()
        fake = FakeStatvfs(HOST_FLOOR_PLUS, HOST_FLOOR)
        err = io.StringIO()
        with patch.object(os, 'statvfs', return_value=fake):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        self.assertEqual(provisioning_findings(err.getvalue()), [],
                         f'exact fit must not warn, stderr={err.getvalue()!r}')
        self.assertEqual(self.leases(), [])

    def test_a10_frsize_scaling_reports_exact_bytes(self):
        # Nontrivial frsize: byte size is blocks * frsize, not the raw block
        # count (pins the scaling factor a mutant would drop).
        self.save_policy(self.host_policy())
        self.make_slots()
        # 24 GiB fits both terms -> silent.
        err = io.StringIO()
        with patch.object(os, 'statvfs',
                          return_value=FakeStatvfs(24 * GIB, HOST_FLOOR, frsize=4096)):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        self.assertEqual(provisioning_findings(err.getvalue()), [],
                         f'24 GiB with frsize must not warn, stderr={err.getvalue()!r}')
        self.assertEqual(self.leases(), [])
        # 20 GiB exceeds floor-plus (22 GiB) via the floor term only, and the
        # reported size must be exact bytes, not the block count.
        err = io.StringIO()
        with patch.object(os, 'statvfs',
                          return_value=FakeStatvfs(20 * GIB, HOST_FLOOR, frsize=4096)):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        findings = provisioning_findings(err.getvalue())
        self.assertEqual(len(findings), 1,
                         f'expected one frsize finding, stderr={err.getvalue()!r}')
        self.assertEqual(findings[0]['filesystem_size_bytes'], 20 * GIB)
        self.assertEqual(self.leases(), [])


class BehaviorPinTests(PoolFixture):
    """B-suite: behavior pins that hold before and after the warning."""

    def test_b1_budgets_over_hard_limit_refuse(self):
        policy = self.small_policy(floor=1, budget=600, hard=1000)
        self.save_policy(policy)
        self.make_slots()
        with self.assertRaisesRegex(cache.Refusal, 'slot budgets exceed'):
            cache.load_policy(self.pool)

    def test_b2_mid_run_floor_trip_retains_lease_and_stops_group(self):
        floor = 4096
        self.save_policy(self.small_policy(floor=floor))
        self.make_slots()
        looper = self.root / 'looper'
        looper.write_text('#!' + sys.executable + '\n'
                          'import time\n'
                          'while True: time.sleep(0.05)\n')
        looper.chmod(0o700)
        calls = []

        def trip_after_admission(path):
            calls.append(str(path))
            return floor if len(calls) == 1 else 0

        with patch.object(cache, 'available', side_effect=trip_after_admission):
            with self.assertRaisesRegex(cache.Refusal, 'floor'):
                cache.run_cargo(self.pool, ['check'], cargo=str(looper), interval=.01)
        self.assertGreaterEqual(len(calls), 2)
        lease_path = self.pool / 'slot-0' / 'lease.json'
        self.assertTrue(lease_path.exists(), 'floor trip must retain the lease')
        pgid = json.loads(lease_path.read_text()).get('cargo_pgid')
        self.assertIsInstance(pgid, int)
        self.assertFalse(cache.group_alive(pgid), 'floor trip must stop the child group')

    def test_b3_floor_uses_pool_filesystem_not_cwd(self):
        floor = 4096
        self.save_policy(self.small_policy(floor=floor))
        self.make_slots()

        def per_path(pool_fake, cwd_fake):
            def fake_statvfs(path):
                if str(path) == str(self.pool):
                    return pool_fake
                return cwd_fake
            return fake_statvfs

        big = 500 * GIB
        # Pool low, cwd high -> pool wins (refuses).
        with patch.object(os, 'statvfs', side_effect=per_path(
                FakeStatvfs(big, 0), FakeStatvfs(big, 100 * GIB))):
            with self.assertRaisesRegex(cache.Refusal, 'insufficient'):
                cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(self.leases(), [])
        # Pool high, cwd low -> pool wins (admits).
        err = io.StringIO()
        with patch.object(os, 'statvfs', side_effect=per_path(
                FakeStatvfs(big, floor), FakeStatvfs(big, 0))):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        self.assertEqual(self.leases(), [])

    def test_b10_no_outer_path_is_consulted(self):
        floor = 4096
        self.save_policy(self.small_policy(floor=floor))
        self.make_slots()
        seen = []
        healthy = FakeStatvfs(500 * GIB, floor)

        def recording(path):
            seen.append(str(path))
            return healthy

        err = io.StringIO()
        with patch.object(os, 'statvfs', side_effect=recording):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        self.assertGreaterEqual(len(seen), 1, 'admission must measure the pool fs')
        self.assertEqual(set(seen), {str(self.pool)},
                         f'pool-scope-only, saw outer paths: {sorted(set(seen))}')

    def test_b5_retain_reclaims_despite_provisioning_mismatch(self):
        # Inconsistent provisioning must never gate recovery: retain reclaims
        # and emits no provisioning finding (guards against a load_policy check).
        self.save_policy(self.host_policy())
        for number in range(2):
            slot = self.pool / f'slot-{number}'
            slot.mkdir(exist_ok=True)
            target = slot / 'target'
            target.mkdir(exist_ok=True)
            (target / 'debug').mkdir(exist_ok=True)
            (target / 'debug' / 'fixture').write_bytes(b'x' * 4096)
            (target / '.rustc_info.json').write_text('{}')
            scratch = slot / 'scratch'
            scratch.mkdir(exist_ok=True)
            (scratch / 'tmp-out').write_bytes(b'x' * 4096)
            (slot / 'lock').touch(exist_ok=True)
            (slot / 'lease.json').write_text(json.dumps(
                {'workspace': str(self.root), 'wrapper_pid': 99999999,
                 'started_at': 0, 'cargo_pgid': 99999998}))
        proc = self.root / 'proc'
        proc.mkdir()
        now = time.time()
        inventory = {'version': 1, 'complete': True, 'process_scope': 'host',
                     'captured_at_unix': now,
                     'slots': [{'path': str(self.pool / f'slot-{n}'),
                                'issue_id': 'fixture-terminal', 'status': 'done',
                                'live_run': False, 'referenced': False,
                                'target_provenance': 'build_output_only'}
                               for n in range(2)]}
        err = io.StringIO()
        with patch.object(os, 'statvfs',
                          return_value=FakeStatvfs(HOST_SIZE, HOST_FLOOR)):
            with contextlib.redirect_stderr(err):
                receipt = cache.shared_pool_retain(self.pool, inventory, proc, now=now)
        self.assertTrue(receipt['retain'])
        by_slot = {row['slot']: row for row in receipt['slots']}
        for number in range(2):
            row = by_slot[f'slot-{number}']
            self.assertTrue(row['eligible'], row)
            self.assertGreater(row['reclaimed_bytes'], 0)
            slot = self.pool / f'slot-{number}'
            self.assertFalse((slot / 'lease.json').exists())
            self.assertEqual(list((slot / 'target').iterdir()), [])
            self.assertEqual(list((slot / 'scratch').iterdir()), [])
        self.assertEqual(provisioning_findings(err.getvalue()), [],
                         f'retain must not warn, stderr={err.getvalue()!r}')

    def test_b6_receipts_refuse_before_any_measurement(self):
        base = self.small_policy()
        cases = [
            ('missing quota', {**base, 'quota_receipt': ''}, 'quota receipt'),
            ('missing scratch', {**base, 'scratch_coverage_receipt': '   '},
             'scratch coverage'),
        ]
        for name, policy, expect in cases:
            with self.subTest(name=name):
                self.save_policy(policy)
                self.make_slots()
                with patch.object(os, 'statvfs') as mocked:
                    mocked.side_effect = AssertionError('must not measure')
                    with self.assertRaisesRegex(cache.Refusal, expect):
                        cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
                    mocked.assert_not_called()
                self.assertEqual(self.leases(), [])

    def test_b7_statvfs_error_fails_closed(self):
        self.save_policy(self.small_policy())
        self.make_slots()
        err = io.StringIO()
        argv = ['cargo_cache.py', 'run', '--pool', str(self.pool), '--', 'check']
        with patch.object(os, 'statvfs', side_effect=OSError('denied')):
            with patch.object(cache, 'resolve_cargo', return_value='/bin/true'):
                with patch.object(sys, 'argv', argv):
                    with contextlib.redirect_stderr(err):
                        self.assertEqual(cache.main(), 75)
        self.assertEqual(self.leases(), [])
        self.assertIn('refused', err.getvalue())

    def test_b8_host_floor_boundary_exact(self):
        self.save_policy(self.host_policy())
        self.make_slots()
        with patch.object(os, 'statvfs',
                          return_value=FakeStatvfs(HOST_SIZE, HOST_FLOOR - 1)):
            with self.assertRaisesRegex(cache.Refusal, 'insufficient'):
                cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(self.leases(), [])
        err = io.StringIO()
        with patch.object(os, 'statvfs',
                          return_value=FakeStatvfs(HOST_SIZE, HOST_FLOOR)):
            with contextlib.redirect_stderr(err):
                rc = cache.run_cargo(self.pool, ['check'], cargo=self.true_bin)
        self.assertEqual(rc, 0, err.getvalue())
        self.assertEqual(self.leases(), [])

    def test_b9_fmt_canary_host_avail_refuses_pre_compile(self):
        self.save_policy(self.host_policy())
        self.make_slots()
        with patch.object(os, 'statvfs',
                          return_value=FakeStatvfs(HOST_SIZE, HOST_AVAIL)):
            with self.assertRaisesRegex(cache.Refusal, 'insufficient'):
                cache.run_cargo(self.pool, ['fmt', '--check'], cargo=self.true_bin)
        self.assertEqual(self.leases(), [])


if __name__ == '__main__':
    unittest.main()
