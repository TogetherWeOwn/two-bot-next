#!/usr/bin/env python3
"""Offline regressions: no Cargo, network, services, or large fixtures."""

import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import cargo_cache as cache


SCRIPT = Path(cache.__file__).resolve()


class CacheTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get('PAPERCLIP_RUN_SCRATCH_DIR'))
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.pool = self.root / 'pool'
        self.pool.mkdir()
        self.policy = {'version': 1, 'slots': 2, 'slot_budget_bytes': 256 * 1024,
                       'min_available_bytes': 1}
        self.save_policy()
        for number in range(2):
            slot = self.pool / f'slot-{number}'
            slot.mkdir()
            (slot / 'target').mkdir()
            (slot / 'lock').touch()
        self.fake = self.root / 'fake-cargo'
        self.fake.write_text('#!' + sys.executable + '\n'
                             'import os,time,pathlib\n'
                             'target=pathlib.Path(os.environ["CARGO_TARGET_DIR"])\n'
                             '(target/"fixture").write_bytes(b"x"*4096)\n'
                             'assert os.environ["CARGO_INCREMENTAL"] == "0"\n'
                             'assert os.environ["CARGO_PROFILE_DEV_DEBUG"] == "0"\n'
                             'assert os.environ["CARGO_PROFILE_TEST_DEBUG"] == "0"\n'
                             '(target/"ready").write_text("ready")\n'
                             'while not (target/"release").exists(): time.sleep(.01)\n')
        self.fake.chmod(0o700)

    def save_policy(self):
        (self.pool / 'policy.json').write_text(json.dumps(self.policy))

    def start_fake(self):
        driver = ('import cargo_cache as c; from pathlib import Path; '
                  'c.run_cargo(Path(__import__("sys").argv[1]), ["check"], '
                  'cargo=__import__("sys").argv[2], interval=.02)')
        env = os.environ.copy()
        env['PYTHONPATH'] = str(SCRIPT.parent)
        child = subprocess.Popen([sys.executable, '-c', driver, str(self.pool), str(self.fake)],
                                 env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

        def finish():
            for slot in self.pool.glob('slot-*'):
                (slot / 'target' / 'release').touch()
            if child.poll() is None:
                child.terminate()
            child.communicate(timeout=5)

        self.addCleanup(finish)
        return child

    def wait_ready(self, count):
        deadline = time.monotonic() + 5
        while len(list(self.pool.glob('slot-*/target/ready'))) < count:
            if time.monotonic() > deadline:
                self.fail('fixture child did not become ready')
            time.sleep(.01)

    def test_two_concurrent_slots_and_third_refused(self):
        first = self.start_fake()
        second = self.start_fake()
        self.wait_ready(2)
        self.assertIsNone(first.poll())
        self.assertIsNone(second.poll())
        with self.assertRaisesRegex(cache.Refusal, 'no idle'):
            cache.acquire(self.pool, self.policy)
        total = sum(cache.usage(p) for p in self.pool.glob('slot-*/target'))
        self.assertLessEqual(total, 2 * self.policy['slot_budget_bytes'])
        print(f'concurrency receipt: 2 running slots, third refused, {total} allocated bytes')
        for slot in self.pool.glob('slot-*'):
            (slot / 'target' / 'release').touch()
        for child in (first, second):
            output, error = child.communicate(timeout=5)
            self.assertEqual(child.returncode, 0, error.decode())
        self.assertEqual(list(self.pool.glob('slot-*/lease.json')), [])
        slot, _, fd = cache.acquire(self.pool, self.policy)
        os.close(fd)
        self.assertEqual(slot.name, 'slot-0')

    def test_guard_stops_growth_and_retains_lease(self):
        self.policy['slot_budget_bytes'] = 24 * 1024
        self.save_policy()
        self.fake.write_text('#!' + sys.executable + '\n'
                             'import os,time,pathlib\n'
                             'p=pathlib.Path(os.environ["CARGO_TARGET_DIR"])/"growing"\n'
                             'with p.open("wb", buffering=0) as f:\n'
                             ' while True: f.write(b"x"*4096); time.sleep(.05)\n')
        with self.assertRaisesRegex(cache.Refusal, 'sampled byte budget'):
            cache.run_cargo(self.pool, ['check'], cargo=str(self.fake), interval=.01)
        self.assertTrue((self.pool / 'slot-0' / 'lease.json').exists())
        self.assertLessEqual(cache.usage(self.pool / 'slot-0' / 'target'), 32 * 1024)

    def test_crash_sentinel_is_not_stolen(self):
        for slot in self.pool.glob('slot-*'):
            (slot / 'lease.json').write_text('{"wrapper_pid": 99999999}')
        with self.assertRaisesRegex(cache.Refusal, 'no idle'):
            cache.acquire(self.pool, self.policy)

    def test_over_budget_idle_slots_are_not_reused(self):
        self.policy['slot_budget_bytes'] = 1
        with self.assertRaisesRegex(cache.Refusal, 'no idle'):
            cache.acquire(self.pool, self.policy)

    def test_low_disk_prevents_launch(self):
        with patch.object(cache, 'available', return_value=0):
            with self.assertRaisesRegex(cache.Refusal, 'insufficient'):
                cache.run_cargo(self.pool, ['check'], cargo=str(self.fake))
        self.assertEqual(list(self.pool.glob('slot-*/lease.json')), [])

    def test_budget_escape_arguments_refused(self):
        for args in (['clean'], ['run'], ['check', '--target-dir=/tmp/other'],
                     ['check', '--config', 'build.target-dir="/tmp/other"'],
                     ['check', '--build-dir=/tmp/other'], ['check', '-Zunstable-options'],
                     ['check', '--manifest-path', '../Cargo.toml']):
            with self.subTest(args=args), self.assertRaises(cache.Refusal):
                cache.validate_cargo_args(args, Path.cwd())

    def test_ambiguous_pool_policy_and_symlink_rejected(self):
        (self.pool / 'extra').mkdir()
        with self.assertRaisesRegex(cache.Refusal, 'immutable policy'):
            cache.load_policy(self.pool)
        alias = self.root / 'alias'
        alias.symlink_to(self.pool)
        with self.assertRaisesRegex(cache.Refusal, 'real directory'):
            cache.real_directory(alias)

    def test_usage_rejects_symlinks_and_counts_blocks(self):
        target = self.pool / 'slot-0' / 'target'
        (target / 'regular').write_bytes(b'x' * 4096)
        self.assertGreaterEqual(cache.usage(target), 4096)
        (target / 'alias').symlink_to(self.root)
        with self.assertRaisesRegex(cache.Refusal, 'symlink'):
            cache.usage(target)

    def test_filesystem_uses_user_available_not_root_free(self):
        class FakeFS:
            f_bavail = 2
            f_bfree = 1000
            f_frsize = 4096

        with patch.object(os, 'statvfs', return_value=FakeFS()):
            self.assertEqual(cache.available(self.root), 8192)
            finding = cache.filesystem_finding(self.root, self.root, 8193)
            self.assertEqual(finding['available_bytes'], 8192)
            self.assertIsNone(cache.filesystem_finding(self.root, self.root, 8192))

    def test_healthy_monitor_is_silent(self):
        out = io.StringIO()
        with patch.object(sys, 'argv', ['cargo_cache.py', 'filesystem', '--path', str(self.root),
                                       '--backing-path', str(self.root), '--min-available-bytes', '1']):
            with contextlib.redirect_stdout(out):
                self.assertEqual(cache.main(), 0)
        self.assertEqual(out.getvalue(), '')


class RetentionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get('PAPERCLIP_RUN_SCRATCH_DIR'))
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.worktrees = self.root / 'worktrees'
        self.worktrees.mkdir()
        self.workspace = self.worktrees / 'terminal-card'
        self.workspace.mkdir()
        subprocess.run(['git', 'init', '-q', str(self.workspace)], check=True)
        (self.workspace / '.gitignore').write_text('/target/\n')
        self.target = self.workspace / 'target'
        self.target.mkdir()
        (self.target / 'fixture').write_bytes(b'x' * 4096)
        (self.workspace / 'evidence.md').write_text('preserve me')
        self.proc = self.root / 'proc'
        self.proc.mkdir()
        self.now = time.time()
        self.row = {'path': str(self.workspace), 'issue_id': 'fixture-terminal',
                    'status': 'done', 'live_run': False, 'referenced': False}
        self.inventory = {'version': 1, 'complete': True, 'process_scope': 'host',
                          'captured_at_unix': self.now, 'workspaces': [self.row]}

    def audit(self):
        return cache.retention_audit(self.worktrees, self.inventory, self.proc, now=self.now)

    def reason(self):
        return self.audit()['candidates'][0]['reason']

    def fake_pid(self):
        pid = self.proc / '123'
        pid.mkdir()
        (pid / 'fd').mkdir()
        (pid / 'stat').write_text('123 (fixture) S 1 123 123 0 -1 0\n')
        (pid / 'cwd').symlink_to(self.root)
        (pid / 'exe').symlink_to(sys.executable)
        (pid / 'maps').write_text('')
        return pid

    def test_terminal_ignored_target_is_audit_only(self):
        receipt = self.audit()
        self.assertTrue(receipt['audit_only'])
        self.assertTrue(receipt['candidates'][0]['eligible'])
        self.assertTrue((self.target / 'fixture').exists())
        self.assertEqual((self.workspace / 'evidence.md').read_text(), 'preserve me')

    def test_live_runs_and_referenced_terminal_workspaces_preserved(self):
        for key in ('live_run', 'referenced'):
            with self.subTest(key=key):
                self.row[key] = True
                self.assertIn('live run', self.reason())
                self.row[key] = False
        self.row['status'] = 'blocked'
        self.assertIn('nonterminal', self.reason())

    def test_missing_stale_ambiguous_and_incomplete_inventory_refused(self):
        for update in ({'complete': False}, {'process_scope': 'container'},
                       {'captured_at_unix': self.now - 61},
                       {'captured_at_unix': self.now + 1},
                       {'workspaces': [self.row, self.row]}):
            with self.subTest(update=update), self.assertRaises(cache.Refusal):
                cache.retention_audit(self.worktrees, self.inventory | update, self.proc, now=self.now)
        self.inventory['workspaces'] = []
        self.assertEqual(self.reason(), 'unattributed')

    def test_tracked_target_preserved(self):
        subprocess.run(['git', '-C', str(self.workspace), 'add', '-f', 'target/fixture'], check=True)
        self.assertEqual(self.reason(), 'tracked or not ignored')

    def test_unignored_target_preserved(self):
        (self.workspace / '.gitignore').write_text('')
        self.assertEqual(self.reason(), 'tracked or not ignored')

    def test_cwd_in_source_workspace_vetoes_terminal_status(self):
        pid = self.fake_pid()
        (pid / 'cwd').unlink()
        (pid / 'cwd').symlink_to(self.workspace)
        self.assertIn('actual process', self.reason())

    def test_open_fd_and_mmap_veto_retention(self):
        pid = self.fake_pid()
        (pid / 'fd' / '7').symlink_to(self.target / 'fixture')
        self.assertIn('actual process', self.reason())
        (pid / 'fd' / '7').unlink()
        info = (self.target / 'fixture').stat()
        # Container path differs from host; dev/inode identity must still veto.
        (pid / 'maps').write_text(f'100-200 r--p 00000000 '
                                 f'{os.major(info.st_dev):x}:{os.minor(info.st_dev):x} '
                                 f'{info.st_ino} /different/container/mount/cache\n')
        self.assertIn('actual process', self.reason())

    def test_denied_process_scan_refuses_all_reclamation(self):
        self.fake_pid()
        with patch.object(os, 'readlink', side_effect=PermissionError('denied')):
            with self.assertRaisesRegex(cache.Refusal, 'visibility'):
                self.audit()

    def test_zombie_and_kernel_thread_have_no_userspace_refs(self):
        pid = self.proc / '123'
        pid.mkdir()
        for suffix in ('Z 1 1 1 0 -1 0', 'S 1 1 1 0 -1 2097152'):
            (pid / 'stat').write_text('123 (fixture) ' + suffix)
            self.assertEqual(cache.process_references(self.proc), [])

    def test_symlink_target_preserved(self):
        (self.target / 'fixture').unlink()
        self.target.rmdir()
        self.target.symlink_to(self.root)
        self.assertEqual(self.reason(), 'symlink')

    def test_shared_cache_outside_worktrees_is_never_a_candidate(self):
        shared = self.root / 'cargo-target-two-bot-next'
        shared.mkdir()
        (shared / 'keep').write_text('shared')
        self.assertEqual(len(self.audit()['candidates']), 1)
        self.assertEqual((shared / 'keep').read_text(), 'shared')


if __name__ == '__main__':
    unittest.main()
