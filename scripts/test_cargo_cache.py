#!/usr/bin/env python3
"""Offline regressions: no Cargo, network, services, or large fixtures."""

import contextlib
import fcntl
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


SCRIPT = Path(cache.__file__).resolve()


class CacheTests(unittest.TestCase):
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
        self.save_policy()
        for number in range(2):
            slot = self.pool / f'slot-{number}'
            slot.mkdir()
            (slot / 'target').mkdir()
            (slot / 'scratch').mkdir()
            (slot / 'lock').touch()
        self.fake = self.root / 'fake-cargo'
        self.fake.write_text('#!' + sys.executable + '\n'
                             'import os,time,pathlib\n'
                             'target=pathlib.Path(os.environ["CARGO_TARGET_DIR"])\n'
                             '(target/"fixture").write_bytes(b"x"*4096)\n'
                             'scratch=pathlib.Path(os.environ["TMPDIR"])\n'
                             'assert scratch == target.parent/"scratch"\n'
                             'assert os.environ["TMP"] == os.environ["TEMP"] == str(scratch)\n'
                             '(scratch/"fixture").write_bytes(b"x"*4096)\n'
                             'assert os.environ["CARGO_INCREMENTAL"] == "0"\n'
                             'assert os.environ["CARGO_PROFILE_DEV_DEBUG"] == "0"\n'
                             'assert os.environ["CARGO_PROFILE_TEST_DEBUG"] == "0"\n'
                             '(target/"ready").write_text("ready")\n'
                             'while not (target/"release").exists(): time.sleep(.01)\n')
        self.fake.chmod(0o700)

    def save_policy(self):
        (self.pool / 'policy.json').write_text(json.dumps(self.policy))

    def start_fake(self, workspace=None):
        driver = ('import cargo_cache as c; from pathlib import Path; '
                  'c.run_cargo(Path(__import__("sys").argv[1]), ["check"], '
                  'cargo=__import__("sys").argv[2], interval=.02)')
        env = os.environ.copy()
        env['PYTHONPATH'] = str(SCRIPT.parent)
        child = subprocess.Popen([sys.executable, '-c', driver, str(self.pool), str(self.fake)],
                                 env=env, cwd=workspace or self.root,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)

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
        workspaces = [self.root / 'isolated-a', self.root / 'isolated-b']
        for workspace in workspaces:
            workspace.mkdir()
        first = self.start_fake(workspaces[0])
        second = self.start_fake(workspaces[1])
        self.wait_ready(2)
        leases = [json.loads(p.read_text()) for p in self.pool.glob('slot-*/lease.json')]
        self.assertEqual({lease['workspace'] for lease in leases}, {str(p) for p in workspaces})
        self.assertFalse(any((p / 'target').exists() for p in workspaces))
        self.assertIsNone(first.poll())
        self.assertIsNone(second.poll())
        with self.assertRaisesRegex(cache.Refusal, 'no idle'):
            cache.acquire(self.pool, self.policy)
        total = sum(cache.usage(p) for p in self.pool.glob('slot-*'))
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

    def dead_pgid(self):
        terminated = subprocess.Popen(['true'])
        terminated.wait()
        self.assertFalse(cache.group_alive(terminated.pid))
        return terminated.pid

    def test_stale_lease_with_free_lock_is_recovered(self):
        slot = self.pool / 'slot-0'
        lease = {'workspace': str(self.root), 'wrapper_pid': 99999999,
                 'started_at': 0, 'cargo_pgid': self.dead_pgid()}
        (slot / 'lease.json').write_text(json.dumps(lease))
        out = io.StringIO()
        with contextlib.redirect_stderr(out):
            got, _, fd = cache.acquire(self.pool, self.policy)
        os.close(fd)
        self.assertEqual(got.name, 'slot-0')
        self.assertFalse((slot / 'lease.json').exists())
        line = json.loads(out.getvalue().strip().splitlines()[-1])
        self.assertEqual(line['finding'], 'stale_lease_recovered')
        self.assertEqual(line['cargo_pgid'], lease['cargo_pgid'])
        print(f"stale recovery receipt: slot-0 lease with dead pgid "
              f"{lease['cargo_pgid']} admitted, sentinel unlinked")

    def test_held_lock_is_skipped_not_recovered(self):
        slot = self.pool / 'slot-0'
        (slot / 'lease.json').write_text(json.dumps(
            {'workspace': str(self.root), 'wrapper_pid': 99999999,
             'started_at': 0, 'cargo_pgid': self.dead_pgid()}))
        held = os.open(slot / 'lock', os.O_RDWR | os.O_NOFOLLOW)
        self.addCleanup(os.close, held)
        fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
        got, _, fd = cache.acquire(self.pool, self.policy)
        os.close(fd)
        self.assertEqual(got.name, 'slot-1')
        self.assertTrue((slot / 'lease.json').exists())

    def test_over_budget_stale_lease_is_still_refused(self):
        dead = self.dead_pgid()
        for slot in self.pool.glob('slot-*'):
            (slot / 'scratch' / 'retained').write_bytes(
                b'x' * self.policy['slot_budget_bytes'])
            (slot / 'lease.json').write_text(json.dumps(
                {'workspace': str(self.root), 'wrapper_pid': 99999999,
                 'started_at': 0, 'cargo_pgid': dead}))
        with self.assertRaisesRegex(cache.Refusal, 'no idle'):
            cache.acquire(self.pool, self.policy)
        self.assertEqual(len(list(self.pool.glob('slot-*/lease.json'))), 2)

    def test_live_recorded_group_is_still_refused(self):
        live = subprocess.Popen(['sleep', '30'], start_new_session=True)

        def finish_live():
            live.terminate()
            try:
                live.wait(timeout=5)
            except subprocess.TimeoutExpired:
                live.kill()
                live.wait(timeout=5)

        self.addCleanup(finish_live)
        self.assertTrue(cache.group_alive(live.pid))
        for slot in self.pool.glob('slot-*'):
            (slot / 'lease.json').write_text(json.dumps(
                {'workspace': str(self.root), 'wrapper_pid': 99999999,
                 'started_at': 0, 'cargo_pgid': live.pid}))
        with self.assertRaisesRegex(cache.Refusal, 'no idle'):
            cache.acquire(self.pool, self.policy)
        self.assertEqual(len(list(self.pool.glob('slot-*/lease.json'))), 2)

    def test_over_budget_idle_slots_are_not_reused(self):
        self.policy['slot_budget_bytes'] = 1
        with self.assertRaisesRegex(cache.Refusal, 'no idle'):
            cache.acquire(self.pool, self.policy)

    def test_absent_quota_attestation_refuses_admission(self):
        del self.policy['quota_receipt']
        self.save_policy()
        with self.assertRaisesRegex(cache.Refusal, 'quota receipt'):
            cache.load_policy(self.pool)

    def test_absent_scratch_coverage_attestation_refuses_admission(self):
        for value in (None, '', '   '):
            with self.subTest(value=value):
                self.policy['scratch_coverage_receipt'] = value
                self.save_policy()
                with self.assertRaisesRegex(cache.Refusal, 'scratch coverage'):
                    cache.run_cargo(self.pool, ['check'], cargo=str(self.fake))
        self.assertEqual(list(self.pool.glob('slot-*/lease.json')), [])

    def test_inherited_external_targets_and_temp_are_overridden_only_in_child(self):
        external = self.root / 'external-scratch'
        external.mkdir()
        overrides = {key: str(external) for key in (
            'CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_BUILD_DIR',
            'TMPDIR', 'TMP', 'TEMP',
            'PAPERCLIP_RUN_SCRATCH_DIR', 'PAPERCLIP_SCRATCH_DIR')}
        self.fake.write_text('#!' + sys.executable + '\n'
                             'import os,pathlib,tempfile\n'
                             'target=pathlib.Path(os.environ["CARGO_TARGET_DIR"])\n'
                             'scratch=target.parent/"scratch"\n'
                             'assert os.environ["CARGO_BUILD_TARGET_DIR"] == str(target)\n'
                             'assert os.environ["CARGO_BUILD_BUILD_DIR"] == str(target)\n'
                             'assert os.environ["TMPDIR"] == os.environ["TMP"] == os.environ["TEMP"]\n'
                             'assert os.environ["TMPDIR"] == str(scratch)\n'
                             'assert os.environ["PAPERCLIP_RUN_SCRATCH_DIR"] == str(scratch)\n'
                             'assert os.environ["PAPERCLIP_SCRATCH_DIR"] == str(scratch)\n'
                             'with tempfile.NamedTemporaryFile() as f:\n'
                             ' assert pathlib.Path(f.name).parent == scratch\n'
                             ' f.write(b"x"*4096)\n')
        with patch.dict(os.environ, overrides):
            self.assertEqual(cache.run_cargo(self.pool, ['check'], cargo=str(self.fake)), 0)
            self.assertEqual({key: os.environ[key] for key in overrides}, overrides)
        self.assertEqual(list(external.iterdir()), [])

    def test_scratch_growth_is_counted_and_stopped(self):
        self.policy['slot_budget_bytes'] = 32 * 1024
        self.save_policy()
        self.fake.write_text('#!' + sys.executable + '\n'
                             'import os,time,pathlib\n'
                             'p=pathlib.Path(os.environ["TMPDIR"])/"growing"\n'
                             'with p.open("wb", buffering=0) as f:\n'
                             ' while True: f.write(b"x"*4096); time.sleep(.05)\n')
        with self.assertRaisesRegex(cache.Refusal, 'sampled byte budget'):
            cache.run_cargo(self.pool, ['check'], cargo=str(self.fake), interval=.01)
        slot = self.pool / 'slot-0'
        self.assertTrue((slot / 'lease.json').exists())
        self.assertEqual(list((slot / 'target').iterdir()), [])
        self.assertGreater(cache.usage(slot), self.policy['slot_budget_bytes'] - 1)
        self.assertLessEqual(cache.usage(slot), 40 * 1024)

    def test_missing_or_symlink_scratch_refuses_admission(self):
        scratch = self.pool / 'slot-0' / 'scratch'
        scratch.rmdir()
        with self.assertRaisesRegex(cache.Refusal, 'real directory'):
            cache.acquire(self.pool, self.policy)
        scratch.symlink_to(self.root)
        with self.assertRaisesRegex(cache.Refusal, 'real directory'):
            cache.acquire(self.pool, self.policy)

    def test_retained_scratch_prevents_reusing_over_budget_slots(self):
        for slot in self.pool.glob('slot-*'):
            (slot / 'scratch' / 'retained').write_bytes(b'x' * self.policy['slot_budget_bytes'])
        with self.assertRaisesRegex(cache.Refusal, 'no idle'):
            cache.acquire(self.pool, self.policy)

    def test_signal_stops_build_and_leaves_crash_sentinel(self):
        child = self.start_fake()
        self.wait_ready(1)
        child.terminate()
        _, error = child.communicate(timeout=5)
        self.assertNotEqual(child.returncode, 0)
        self.assertIn(b'interrupted by signal', error)
        self.assertTrue((self.pool / 'slot-0' / 'lease.json').exists())

    def test_repeated_signal_during_shutdown_still_kills_group(self):
        # A fake Cargo that ignores SIGTERM, with the driver sent a second
        # SIGTERM during the wait: shutdown must still SIGKILL the group and
        # retain the crash sentinel instead of raising out of cleanup.
        self.fake.write_text('#!' + sys.executable + '\n'
                             'import os,signal,time,pathlib\n'
                             'signal.signal(signal.SIGTERM, signal.SIG_IGN)\n'
                             'signal.signal(signal.SIGINT, signal.SIG_IGN)\n'
                             'target=pathlib.Path(os.environ["CARGO_TARGET_DIR"])\n'
                             '(target/"ready").write_text("ready")\n'
                             'while not (target/"release").exists(): time.sleep(.01)\n')
        child = self.start_fake()
        self.wait_ready(1)
        child.terminate()
        time.sleep(.2)
        child.terminate()  # second signal lands during cleanup
        _, error = child.communicate(timeout=10)
        self.assertNotEqual(child.returncode, 0)
        self.assertIn(b'interrupted by signal', error)
        self.assertTrue((self.pool / 'slot-0' / 'lease.json').exists())

    def test_second_signal_before_handler_ignore_is_noop(self):
        # Deterministic transition fixture for the exact window a real second
        # signal can hit: after cancellation starts but before the ignores
        # take effect. The first SIGTERM starts cleanup only after the fake
        # reports ready, so cancellation handlers are provably installed (no
        # fixed timer can fire before arming and kill the suite on a delayed
        # runner); the _before_stop seam delivers a second SIGTERM while the
        # raising handler is still installed. Later signals must be no-ops
        # from the first cancellation onward, so cleanup still reaches
        # stop_group, the SIGTERM-ignoring fake is SIGKILLed, and the
        # sentinel is retained.
        import signal as sigmod
        import threading
        self.fake.write_text('#!' + sys.executable + '\n'
                             'import os,signal,time,pathlib\n'
                             'signal.signal(signal.SIGTERM, signal.SIG_IGN)\n'
                             'signal.signal(signal.SIGINT, signal.SIG_IGN)\n'
                             'target=pathlib.Path(os.environ["CARGO_TARGET_DIR"])\n'
                             '(target/"ready").write_text("ready")\n'
                             'while not (target/"release").exists(): time.sleep(.01)\n')

        def send_after_ready():
            deadline = time.monotonic() + 10
            while not any((slot / 'ready').exists()
                          for slot in self.pool.glob('slot-*/target')):
                if time.monotonic() > deadline:
                    return
                time.sleep(.01)
            os.kill(os.getpid(), sigmod.SIGTERM)

        before = sigmod.getsignal(sigmod.SIGTERM)
        sender = threading.Thread(target=send_after_ready, daemon=True)
        sender.start()
        try:
            with self.assertRaisesRegex(cache.Refusal, 'interrupted by signal'):
                cache.run_cargo(
                    self.pool, ['check'], cargo=str(self.fake), interval=.01,
                    _before_stop=lambda: os.kill(os.getpid(), sigmod.SIGTERM))
        finally:
            sender.join(timeout=10)
        self.assertIs(sigmod.getsignal(sigmod.SIGTERM), before)
        self.assertTrue((self.pool / 'slot-0' / 'lease.json').exists())

    def test_signal_during_spawn_stops_group_and_retains_sentinel(self):
        # Launch-transition fixture: SIGTERM lands inside Popen, before the
        # child handle exists. Cancellation is already armed, so the signal
        # is recorded instead of raising out of spawn internals (which would
        # leave child=None and bypass cleanup) or hitting the default
        # handler. The wrapper stops the group on the pending cancellation
        # and retains the crash sentinel.
        import signal as sigmod
        real_popen = subprocess.Popen

        def signalling_popen(*args, **kwargs):
            os.kill(os.getpid(), sigmod.SIGTERM)
            return real_popen(*args, **kwargs)

        before = sigmod.getsignal(sigmod.SIGTERM)
        try:
            with patch.object(subprocess, 'Popen', signalling_popen):
                with self.assertRaisesRegex(cache.Refusal, 'interrupted by signal'):
                    cache.run_cargo(self.pool, ['check'], cargo=str(self.fake),
                                    interval=.01)
        finally:
            self.assertIs(sigmod.getsignal(sigmod.SIGTERM), before)
        self.assertTrue((self.pool / 'slot-0' / 'lease.json').exists())

    def test_low_disk_prevents_launch(self):
        with patch.object(cache, 'available', return_value=0):
            with self.assertRaisesRegex(cache.Refusal, 'insufficient'):
                cache.run_cargo(self.pool, ['check'], cargo=str(self.fake))
        self.assertEqual(list(self.pool.glob('slot-*/lease.json')), [])

    def no_cargo_environment(self):
        # PATH, CARGO_HOME and HOME all point at empty directories.
        empty = self.root / 'no-cargo'
        empty.mkdir(exist_ok=True)
        return patch.dict(os.environ, PATH=str(empty), CARGO_HOME=str(empty), HOME=str(empty))

    def assert_slots_reusable(self):
        self.assertEqual(list(self.pool.glob('slot-*/lease.json')), [])
        slot, _, fd = cache.acquire(self.pool, self.policy)
        os.close(fd)
        self.assertEqual(slot.name, 'slot-0')

    def test_missing_cargo_is_refused_before_leasing(self):
        # TOG-11995: agent PATH had no cargo, so every run wedged a slot.
        with self.no_cargo_environment():
            with patch.object(cache, 'acquire', side_effect=AssertionError('pool touched')):
                with self.assertRaisesRegex(cache.Refusal, 'not found on PATH'):
                    cache.run_cargo(self.pool, ['check'])
            err = io.StringIO()
            with patch.object(sys, 'argv', ['cargo_cache.py', 'run', '--pool', str(self.pool),
                                           '--', 'check']):
                with contextlib.redirect_stderr(err):
                    self.assertEqual(cache.main(), 75)
            self.assertIn('not found on PATH', err.getvalue())
        self.assert_slots_reusable()

    def test_cargo_resolves_from_cargo_home_then_home(self):
        self.fake.write_text('#!' + sys.executable + '\n'
                             'import os,sys\n'
                             'path=os.environ["PATH"].split(os.pathsep)\n'
                             'sys.exit(0 if path[0]==os.path.dirname(sys.argv[0]) else 3)\n')
        with self.no_cargo_environment():
            for variable in ('CARGO_HOME', 'HOME'):
                home = self.root / f'from-{variable}'
                bindir = home / 'bin' if variable == 'CARGO_HOME' else home / '.cargo' / 'bin'
                bindir.mkdir(parents=True)
                shutil.copy2(self.fake, bindir / 'cargo')
                with self.subTest(variable=variable), patch.dict(os.environ, {variable: str(home)}):
                    self.assertEqual(cache.resolve_cargo('cargo'), str(bindir / 'cargo'))
                    # The child PATH gains the proxy directory; the parent's does not.
                    self.assertEqual(cache.run_cargo(self.pool, ['check']), 0)
                    self.assertNotIn(str(bindir), os.environ['PATH'])
                    self.assert_slots_reusable()
            self.assertEqual(cache.resolve_cargo(str(self.fake)), str(self.fake))
            with self.assertRaises(cache.Refusal):
                cache.resolve_cargo(str(self.root / 'missing' / 'cargo'))

    def test_failed_spawn_releases_its_own_lease(self):
        # Popen raising OSError means exec/fork failed and no writer ever
        # existed, while this process still holds the slot flock: the
        # wrapper removes its own sentinel. Covers a mocked FileNotFoundError
        # and a real exec failure (executable with a missing interpreter).
        import signal as sigmod
        lease = self.pool / 'slot-0' / 'lease.json'

        def failing_popen(*args, **kwargs):
            self.assertTrue(lease.exists(), 'lease must exist before spawn')
            raise FileNotFoundError(2, 'No such file or directory', args[0][0])

        broken = self.root / 'broken-cargo'
        broken.write_text('#!' + str(self.root / 'missing-interpreter') + '\n')
        broken.chmod(0o700)
        before = sigmod.getsignal(sigmod.SIGTERM)
        for name, popen in (('mocked', failing_popen), ('real', subprocess.Popen)):
            with self.subTest(name), patch.object(subprocess, 'Popen', popen):
                with self.assertRaises(FileNotFoundError):
                    cache.run_cargo(self.pool, ['check'], cargo=str(broken))
                self.assertIs(sigmod.getsignal(sigmod.SIGTERM), before)
                self.assert_slots_reusable()

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
        (self.target / 'debug').mkdir(parents=True)
        (self.target / 'debug' / 'fixture').write_bytes(b'x' * 4096)
        (self.workspace / 'evidence.md').write_text('preserve me')
        self.proc = self.root / 'proc'
        self.proc.mkdir()
        self.now = time.time()
        self.row = {'path': str(self.workspace), 'issue_id': 'fixture-terminal',
                    'status': 'done', 'live_run': False, 'referenced': False,
                    'target_provenance': 'build_output_only'}
        self.inventory = {'version': 1, 'complete': True, 'process_scope': 'host',
                          'captured_at_unix': self.now, 'workspaces': [self.row]}

    def audit(self):
        return cache.retention_audit(self.worktrees, self.inventory, self.proc, now=self.now)

    def reason(self):
        return self.audit()['candidates'][0]['reason']

    def make_workspace(self, name):
        # A second ignored-target workspace that reaches every gate the
        # fixture workspace reaches, so multi-row tests use one row per
        # distinct canonical path.
        workspace = self.worktrees / name
        workspace.mkdir()
        subprocess.run(['git', 'init', '-q', str(workspace)], check=True)
        (workspace / '.gitignore').write_text('/target/\n')
        target = workspace / 'target'
        (target / 'debug').mkdir(parents=True)
        (target / 'debug' / 'fixture').write_bytes(b'x' * 4096)
        return workspace

    def by_target(self):
        return {row['target']: row for row in self.audit()['candidates']}

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
        self.assertTrue((self.target / 'debug' / 'fixture').exists())
        self.assertEqual((self.workspace / 'evidence.md').read_text(), 'preserve me')

    def test_unclassified_or_foreign_target_is_ineligible(self):
        # P1: filename heuristics cannot mint provenance. An attested
        # Operator build-output-only classification is required; these three
        # foreign files all pass the name checks (debug subtree, no
        # protected stem/suffix), so without the classification gate they
        # would be reported eligible.
        (self.target / 'debug' / 'incident-20260930.json').write_bytes(b'x' * 64)
        (self.target / 'debug' / 'snapshot.tar.gz').write_bytes(b'x' * 64)
        (self.target / 'debug' / 'src').mkdir()
        (self.target / 'debug' / 'src' / 'main.py').write_bytes(b'x' * 64)
        extras = [self.make_workspace(f'provenance-{provenance}')
                  for provenance in ('unclassified', 'mixed', 'unknown')]
        self.inventory['workspaces'] = [self.row] + [
            dict(self.row, path=str(workspace), target_provenance=provenance)
            for workspace, provenance in
            zip(extras, ('unclassified', 'mixed', 'unknown'))]
        by_target = self.by_target()
        for workspace in extras:
            candidate = by_target[str(workspace / 'target')]
            self.assertFalse(candidate['eligible'])
            self.assertIn('provenance', candidate['reason'])
        self.assertTrue(by_target[str(self.target)]['eligible'])
        self.inventory['workspaces'] = [self.row]
        for workspace in extras:
            shutil.rmtree(workspace)
        key = str(self.target)
        self.assertTrue(self.by_target()[key]['eligible'])

        def primary_reason():
            return self.by_target()[key]['reason']

        # Build-output-only heuristics remain as a backstop: material the
        # heuristics do catch still vetoes even with a classification.
        (self.target / 'evidence').mkdir()
        (self.target / 'evidence' / 'incident.md').write_bytes(b'x' * 64)
        self.assertIn('preserved', primary_reason())
        (self.target / 'evidence' / 'incident.md').unlink()
        (self.target / 'evidence').rmdir()
        (self.target / 'debug' / 'stashed-source.rs').write_bytes(b'x' * 64)
        self.assertIn('preserved', primary_reason())
        (self.target / 'debug' / 'stashed-source.rs').unlink()
        (self.target / 'incident-report.md').write_bytes(b'x' * 64)
        self.assertIn('preserved', primary_reason())
        (self.target / 'incident-report.md').unlink()
        self.assertTrue(self.by_target()[key]['eligible'])

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

    def test_noncanonical_and_aliased_inventory_rows_refused(self):
        # '/worktrees/./a' spells the same directory as '/worktrees/a'; raw
        # noncanonical paths are refused outright, as are duplicate canonical
        # identities with conflicting live/reference rows.
        alias = str(self.workspace) + '/.'
        self.inventory['workspaces'] = [self.row | {'path': alias}]
        with self.assertRaisesRegex(cache.Refusal, 'ambiguous'):
            self.audit()
        live = self.row | {'live_run': True, 'referenced': True}
        self.inventory['workspaces'] = [self.row, live]
        with self.assertRaisesRegex(cache.Refusal, 'ambiguous'):
            self.audit()

    def test_conflicting_symlink_inventory_rows_refused(self):
        # Symlink/bind-mount aliases share (st_dev, st_ino) with the real
        # workspace. A terminal row under the real path and a live/queued row
        # under the alias — with no process entry — must refuse the whole
        # audit, not select only the terminal row.
        alias_dir = self.root / 'alias-worktrees'
        alias_dir.mkdir()
        link = alias_dir / 'terminal-card'
        link.symlink_to(self.workspace, target_is_directory=True)
        live = self.row | {'path': str(link), 'live_run': True,
                           'referenced': True, 'status': 'in_progress'}
        self.inventory['workspaces'] = [self.row, live]
        with self.assertRaisesRegex(cache.Refusal, 'ambiguous'):
            self.audit()
        # Symlink spellings are refused outright.
        self.inventory['workspaces'] = [self.row | {'path': str(link)}]
        with self.assertRaisesRegex(cache.Refusal, 'ambiguous'):
            self.audit()

    def test_tracked_target_preserved(self):
        subprocess.run(['git', '-C', str(self.workspace), 'add', '-f', 'target/debug/fixture'], check=True)
        self.assertEqual(self.reason(), 'tracked or not ignored')

    def test_alternate_git_index_cannot_hide_tracked_output(self):
        # -C does not neutralize an inherited GIT_INDEX_FILE: an alternate
        # empty index would hide a force-tracked target while the ignore
        # rule still passes. The audit must inspect the real index and keep
        # the candidate ineligible.
        subprocess.run(['git', '-C', str(self.workspace), 'add', '-f', 'target/debug/fixture'], check=True)
        empty = self.root / 'empty-index'
        empty.touch()
        before = os.environ.get('GIT_INDEX_FILE')
        with patch.dict(os.environ, {'GIT_INDEX_FILE': str(empty)}):
            self.assertEqual(self.reason(), 'tracked or not ignored')
        self.assertEqual(os.environ.get('GIT_INDEX_FILE'), before)

    def test_bind_aliased_target_reconciled_across_rows(self):
        # Two distinct canonical roots sharing one bind-aliased target: a
        # terminal row under A and a live row under B. Without target
        # reconciliation the shared output would be offered as a retention
        # candidate; with it the whole audit refuses. A symlink stands in
        # for the bind mount: os.stat follows it, so both inventory rows
        # report the same target identity while the workspace roots stay
        # distinct and canonical.
        other = self.make_workspace('live-alias-root')
        shutil.rmtree(other / 'target')
        (other / 'target').symlink_to(self.target, target_is_directory=True)
        live = self.row | {'path': str(other), 'live_run': True,
                           'referenced': True, 'status': 'in_progress'}
        self.inventory['workspaces'] = [self.row, live]
        with self.assertRaisesRegex(cache.Refusal, 'ambiguous target'):
            self.audit()

    def test_non_file_maps_deleted_clears_audit(self):
        # The read-only audit applies the same identity exclusion, so a host
        # whose only unattributed deleted references are SYSV shm mappings
        # does not refuse the whole audit. Per-candidate classification stays
        # strict (the entry still marks its candidate ineligible); only the
        # global refusal is lifted.
        shm = os.makedev(0x00, 0x01)
        with patch.object(cache, 'process_references',
                          return_value=([], [('/SYSV00000000', shm, 1, False)])):
            receipt = self.audit()
        self.assertIn('candidates', receipt)

    def test_deleted_container_reference_vetoes_or_refuses(self):
        # An unlinked artifact opened under a container spelling matches
        # neither the host path nor a live workspace inode. When the path
        # reads inside the candidate workspace it vetoes that candidate;
        # when it is attributable to nothing, the whole audit refuses.
        # A dangling fixture symlink would be skipped by the exit-race
        # guard, so simulate the kernel's ' (deleted)' readlink suffix
        # with a live target (real /proc fd stat succeeds on unlinked
        # open files too) and exercise the attribution branches exactly.
        pid = self.fake_pid()
        gone = self.target / 'debug' / 'replaced.so'
        gone.write_bytes(b'x' * 64)
        (pid / 'fd' / '7').symlink_to(gone)
        real_readlink = os.readlink

        def deleted_readlink(link):
            if str(link).endswith('fd/7'):
                return str(gone) + ' (deleted)'
            return real_readlink(link)

        with patch.object(cache.os, 'readlink', side_effect=deleted_readlink):
            self.assertIn('deleted', self.reason())
        stale = self.proc / '999'
        stale.mkdir()
        (stale / 'fd').mkdir()
        (stale / 'stat').write_text('999 (fixture) S 1 999 999 0 -1 0\n')
        (stale / 'cwd').symlink_to(self.root)
        (stale / 'exe').symlink_to(sys.executable)
        (stale / 'maps').write_text('100-200 r--p 00000000 00:01 999999991 '
                                    '/different/container/mount/stale.so (deleted)\n')
        with self.assertRaisesRegex(cache.Refusal, 'unresolved deleted'):
            self.audit()

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
        (pid / 'fd' / '7').symlink_to(self.target / 'debug' / 'fixture')
        self.assertIn('actual process', self.reason())
        (pid / 'fd' / '7').unlink()
        info = (self.target / 'debug' / 'fixture').stat()
        # Container path differs from host; dev/inode identity must still veto.
        (pid / 'maps').write_text(f'100-200 r--p 00000000 '
                                 f'{os.major(info.st_dev):x}:{os.minor(info.st_dev):x} '
                                 f'{info.st_ino} /different/container/mount/cache\n')
        self.assertIn('actual process', self.reason())

    def test_real_linux_process_fd_and_mmap_are_detected(self):
        child = subprocess.Popen([sys.executable, '-c',
                                  'import mmap,sys,time; '
                                  'f=open(sys.argv[1], "rb"); '
                                  'm=mmap.mmap(f.fileno(),0,access=mmap.ACCESS_READ); '
                                  'print("ready",flush=True); time.sleep(30)',
                                  str(self.target / 'debug' / 'fixture')], cwd=self.root,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)

        def finish():
            child.terminate()
            child.communicate(timeout=5)

        self.addCleanup(finish)
        self.assertEqual(child.stdout.readline().strip(), b'ready')
        (self.proc / str(child.pid)).symlink_to(Path('/proc') / str(child.pid))
        self.assertIn('actual process', self.reason())

    def test_denied_process_scan_refuses_all_reclamation(self):
        self.fake_pid()
        with patch.object(os, 'readlink', side_effect=PermissionError('denied')):
            with self.assertRaisesRegex(cache.Refusal, 'visibility'):
                self.audit()

    def test_zombie_and_kernel_thread_have_no_userspace_refs(self):
        pid = self.proc / '123'
        pid.mkdir()
        (pid / 'task').mkdir()
        (pid / 'task' / '1').mkdir()
        for suffix in ('Z 1 1 1 0 -1 0', 'S 1 1 1 0 -1 2097152'):
            (pid / 'stat').write_text('123 (fixture) ' + suffix)
            refs, deleted = cache.process_references(self.proc)
            self.assertEqual((refs, deleted), ([], []))

    def test_multithreaded_zombie_group_refuses_audit(self):
        # A Z leader with surviving worker tasks may still hold workspace
        # references through its workers; the leader state alone must not
        # establish group death.
        pid = self.proc / '123'
        pid.mkdir()
        (pid / 'task').mkdir()
        (pid / 'task' / '1').mkdir()
        (pid / 'task' / '2').mkdir()
        (pid / 'stat').write_text('123 (fixture) Z 1 1 1 0 -1 0')
        with self.assertRaisesRegex(cache.Refusal, 'zombie pid 123'):
            self.audit()

    def test_symlink_target_preserved(self):
        (self.target / 'debug' / 'fixture').unlink()
        (self.target / 'debug').rmdir()
        self.target.rmdir()
        self.target.symlink_to(self.root)
        self.assertEqual(self.reason(), 'symlink')

    def test_external_scratch_targets_are_not_retention_candidates(self):
        scratch = self.root / 'container-tmp'
        scratch.mkdir()
        for name in ('tog-10078-fixes-target', 'tog-10078-db-target', 'two-bot-next-s2'):
            target = scratch / name
            target.mkdir()
            (target / 'fixture').write_bytes(b'x' * 4096)
        receipt = self.audit()
        self.assertEqual([row['target'] for row in receipt['candidates']], [str(self.target)])
        self.assertEqual(len(list(scratch.glob('*/fixture'))), 3)

    def test_shared_cache_outside_worktrees_is_never_a_candidate(self):
        shared = self.root / 'cargo-target-two-bot-next'
        shared.mkdir()
        (shared / 'keep').write_text('shared')
        self.assertEqual(len(self.audit()['candidates']), 1)
        self.assertEqual((shared / 'keep').read_text(), 'shared')


class SharedPoolRetainTests(unittest.TestCase):
    """Shared-pool lock-through-mutation retention: exact slot paths only."""

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

    def by_slot(self, receipt):
        return {row['slot']: row for row in receipt['slots']}

    def fake_pid(self):
        pid = self.proc / '123'
        if not pid.exists():
            pid.mkdir()
            (pid / 'fd').mkdir()
            (pid / 'stat').write_text('123 (fixture) S 1 123 123 0 -1 0\n')
            (pid / 'cwd').symlink_to(self.root)
            (pid / 'exe').symlink_to(sys.executable)
            (pid / 'maps').write_text('')
        return pid

    def test_eligible_slots_reclaimed_lock_inode_preserved(self):
        locks_before = {n: (self.pool / f'slot-{n}' / 'lock').stat()
                        for n in range(2)}
        policy_before = (self.pool / 'policy.json').read_bytes()
        receipt = self.retain()
        self.assertTrue(receipt['retain'])
        by_slot = self.by_slot(receipt)
        for number in range(2):
            row = by_slot[f'slot-{number}']
            self.assertTrue(row['eligible'], row)
            self.assertGreater(row['reclaimed_bytes'], 0)
            self.assertTrue(row['lock_held_through_mutation'])
            slot = self.pool / f'slot-{number}'
            self.assertFalse((slot / 'lease.json').exists())
            self.assertEqual(list((slot / 'target').iterdir()), [])
            self.assertEqual(list((slot / 'scratch').iterdir()), [])
            after = (slot / 'lock').stat()
            self.assertEqual((after.st_dev, after.st_ino),
                             (locks_before[number].st_dev, locks_before[number].st_ino))
        self.assertEqual((self.pool / 'policy.json').read_bytes(), policy_before)

    def test_lock_held_through_mutation(self):
        seen = {}

        def hook(slot, fd):
            fd_stat = os.fstat(fd)
            path_stat = os.stat(slot / 'lock')
            seen[slot.name] = ((fd_stat.st_dev, fd_stat.st_ino)
                               == (path_stat.st_dev, path_stat.st_ino))
            probe = os.open(slot / 'lock', os.O_RDWR | os.O_NOFOLLOW)
            try:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(probe, fcntl.LOCK_EX | fcntl.LOCK_NB)
            finally:
                os.close(probe)

        receipt = cache.shared_pool_retain(self.pool, self.inventory, self.proc,
                                           now=self.now, _before_mutation=hook)
        self.assertEqual(seen, {'slot-0': True, 'slot-1': True})
        self.assertTrue(all(row['eligible'] for row in receipt['slots']))

    def test_lock_replaced_before_mutation_skips_without_deletion(self):
        def hook(slot, fd):
            if slot.name == 'slot-0':
                (slot / 'lock').unlink()
                (slot / 'lock').touch()

        receipt = cache.shared_pool_retain(self.pool, self.inventory, self.proc,
                                           now=self.now, _before_mutation=hook)
        by_slot = self.by_slot(receipt)
        self.assertFalse(by_slot['slot-0']['eligible'])
        self.assertIn('lock', by_slot['slot-0']['reason'])
        self.assertTrue((self.pool / 'slot-0' / 'target' / 'debug' / 'fixture').exists())
        self.assertTrue(by_slot['slot-1']['eligible'])

    def test_held_lock_skips_and_keeps_lease(self):
        slot = self.pool / 'slot-0'
        held = os.open(slot / 'lock', os.O_RDWR | os.O_NOFOLLOW)
        self.addCleanup(os.close, held)
        fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
        receipt = self.retain()
        by_slot = self.by_slot(receipt)
        self.assertFalse(by_slot['slot-0']['eligible'])
        self.assertIn('lock held', by_slot['slot-0']['reason'])
        self.assertTrue((slot / 'lease.json').exists())
        self.assertTrue(by_slot['slot-1']['eligible'])

    def test_target_and_scratch_provenance_vetoes_skip(self):
        slot = self.pool / 'slot-0'
        target = slot / 'target'
        (target / 'evidence').mkdir()
        (target / 'evidence' / 'incident.md').write_bytes(b'x' * 64)
        receipt = self.retain()
        by_slot = self.by_slot(receipt)
        self.assertFalse(by_slot['slot-0']['eligible'])
        self.assertIn('preserved', by_slot['slot-0']['reason'])
        self.assertTrue((slot / 'lease.json').exists())
        self.assertTrue(by_slot['slot-1']['eligible'])
        shutil.rmtree(target / 'evidence')
        (target / 'debug' / 'stashed-source.rs').write_bytes(b'x' * 64)
        receipt = self.retain()
        self.assertIn('preserved', self.by_slot(receipt)['slot-0']['reason'])
        (target / 'debug' / 'stashed-source.rs').unlink()
        (target / 'foreign-top').write_text('foreign')
        receipt = self.retain()
        self.assertIn('foreign', self.by_slot(receipt)['slot-0']['reason'])
        (target / 'foreign-top').unlink()
        scratch = slot / 'scratch'
        (scratch / 'stashed-source.rs').write_bytes(b'x' * 64)
        receipt = self.retain()
        self.assertIn('preserved', self.by_slot(receipt)['slot-0']['reason'])
        (scratch / 'stashed-source.rs').unlink()
        (scratch / 'evidence').mkdir()
        receipt = self.retain()
        self.assertIn('preserved', self.by_slot(receipt)['slot-0']['reason'])

    def test_unclassified_live_referenced_unattributed_skip(self):
        self.inventory['slots'][0]['target_provenance'] = 'mixed'
        self.assertIn('provenance', self.by_slot(self.retain())['slot-0']['reason'])
        self.inventory['slots'][0]['target_provenance'] = 'build_output_only'
        self.inventory['slots'][0]['live_run'] = True
        self.assertIn('live run', self.by_slot(self.retain())['slot-0']['reason'])
        self.inventory['slots'][0]['live_run'] = False
        self.inventory['slots'][0]['referenced'] = True
        self.assertIn('reference', self.by_slot(self.retain())['slot-0']['reason'])
        self.inventory['slots'][0]['referenced'] = False
        self.inventory['slots'][0]['status'] = 'in_progress'
        self.assertIn('nonterminal', self.by_slot(self.retain())['slot-0']['reason'])
        self.inventory['slots'][0]['status'] = 'done'
        del self.inventory['slots'][0]
        row = self.by_slot(self.retain())['slot-0']
        self.assertFalse(row['eligible'])
        self.assertIn('unattributed', row['reason'])

    def test_ambiguous_and_stale_inventory_refuses_whole_without_mutation(self):
        for update in ({'complete': False}, {'process_scope': 'container'},
                       {'captured_at_unix': self.now - 61},
                       {'captured_at_unix': self.now + 1},
                       {'slots': 'not-a-list'}):
            with self.subTest(update=update), self.assertRaises(cache.Refusal):
                cache.shared_pool_retain(self.pool, self.inventory | update,
                                         self.proc, now=self.now)
        dup = dict(self.inventory['slots'][0])
        with self.assertRaisesRegex(cache.Refusal, 'ambiguous'):
            cache.shared_pool_retain(self.pool,
                                     self.inventory | {'slots': [dup, dup]},
                                     self.proc, now=self.now)
        alias = str(self.pool / 'slot-0') + '/.'
        with self.assertRaisesRegex(cache.Refusal, 'ambiguous'):
            cache.shared_pool_retain(
                self.pool,
                self.inventory | {'slots': [self.row('slot-0') | {'path': alias},
                                            self.row('slot-1')]},
                self.proc, now=self.now)
        # No mutation on whole-run refusal.
        self.assertTrue((self.pool / 'slot-0' / 'target' / 'debug' / 'fixture').exists())
        self.assertTrue((self.pool / 'slot-0' / 'lease.json').exists())

    def test_process_cwd_fd_mmap_cmdline_veto_retention(self):
        pid = self.fake_pid()
        target = self.pool / 'slot-0' / 'target'
        (pid / 'cwd').unlink()
        (pid / 'cwd').symlink_to(target / 'debug')
        row = self.by_slot(self.retain())['slot-0']
        self.assertFalse(row['eligible'])
        self.assertIn('process', row['reason'])
        (pid / 'cwd').unlink()
        (pid / 'cwd').symlink_to(self.root)
        (pid / 'fd' / '7').symlink_to(target / 'debug' / 'fixture')
        row = self.by_slot(self.retain())['slot-0']
        self.assertFalse(row['eligible'])
        self.assertIn('process', row['reason'])
        (pid / 'fd' / '7').unlink()
        info = (target / 'debug' / 'fixture').stat()
        (pid / 'maps').write_text(
            f'100-200 r--p 00000000 {os.major(info.st_dev):x}:{os.minor(info.st_dev):x} '
            f'{info.st_ino} /different/container/mount/cache\n')
        row = self.by_slot(self.retain())['slot-0']
        self.assertFalse(row['eligible'])
        self.assertIn('process', row['reason'])
        (pid / 'maps').write_text('')
        (pid / 'cmdline').write_bytes(str(target / 'debug' / 'fixture').encode() + b'\0')
        row = self.by_slot(self.retain())['slot-0']
        self.assertFalse(row['eligible'])
        self.assertIn('process', row['reason'])
        (pid / 'cmdline').write_bytes(b'/usr/bin/python\0')
        by_slot = self.by_slot(self.retain())
        self.assertTrue(by_slot['slot-0']['eligible'])
        self.assertTrue(by_slot['slot-1']['eligible'])

    def test_cmdline_lexical_only_reference_vetoes(self):
        pid = self.fake_pid()
        missing = self.pool / 'slot-0' / 'target' / 'debug' / 'future-output'
        self.assertFalse(missing.exists())
        (pid / 'cmdline').write_bytes(str(missing).encode() + b'\0')
        by_slot = self.by_slot(self.retain())
        self.assertFalse(by_slot['slot-0']['eligible'])
        self.assertTrue(by_slot['slot-1']['eligible'])

    def test_deleted_artifact_vetoes_or_refuses_whole(self):
        pid = self.fake_pid()
        target = self.pool / 'slot-0' / 'target'
        gone = target / 'debug' / 'replaced.so'
        gone.write_bytes(b'x' * 64)
        (pid / 'fd' / '7').symlink_to(gone)
        real_readlink = os.readlink

        def deleted_readlink(link):
            if str(link).endswith('fd/7'):
                return str(gone) + ' (deleted)'
            return real_readlink(link)

        with patch.object(cache.os, 'readlink', side_effect=deleted_readlink):
            row = self.by_slot(self.retain())['slot-0']
            self.assertFalse(row['eligible'])
            self.assertIn('deleted', row['reason'])
        (pid / 'fd' / '7').unlink()
        stale = self.proc / '999'
        stale.mkdir()
        (stale / 'fd').mkdir()
        (stale / 'stat').write_text('999 (fixture) S 1 999 999 0 -1 0\n')
        (stale / 'cwd').symlink_to(self.root)
        (stale / 'exe').symlink_to(sys.executable)
        # Same-filesystem deleted entry outside every slot path, with an
        # inode in no slot traversal: indistinguishable from a deleted slot
        # file held open, so the whole run still refuses with no mutation.
        # Maps provenance, so device comparison could not exclude it anyway.
        pool_dev = os.stat(self.pool / 'slot-0' / 'target').st_dev
        (stale / 'maps').write_text(
            f'100-200 r--p 00000000 {os.major(pool_dev):x}:{os.minor(pool_dev):x} '
            f'999999991 /different/container/mount/stale.so (deleted)\n')
        with self.assertRaisesRegex(cache.Refusal, 'unresolved deleted'):
            self.retain()
        self.assertTrue((target / 'debug' / 'fixture').exists())

    def test_different_filesystem_deleted_excluded(self):
        # Multi-tenant host shape: unrelated stat-backed (fd/cwd/exe) deleted
        # artifacts on filesystems holding no slot output are provably unable
        # to alias slot output, so they are excluded (and counted) instead of
        # refusing the whole run.
        observed = {os.stat(self.pool / f'slot-{n}' / sub).st_dev
                    for n in range(2) for sub in ('target', 'scratch')}
        foreign = os.makedev(0xAB, 0xCD)
        self.assertNotIn(foreign, observed)
        with patch.object(cache, 'process_references',
                          return_value=([], [('/other/tenant/stale.so', foreign, 999999991, True),
                                             ('/other/tenant/old.so', foreign, 999999992, True)])):
            receipt = self.retain()
        self.assertTrue(all(row['eligible'] for row in receipt['slots']))
        self.assertEqual(receipt['excluded_deleted_references'], 2)
        self.assertFalse((self.pool / 'slot-0' / 'lease.json').exists())

    def test_maps_deleted_never_device_excluded(self):
        # Maps devices are kernel-printed superblock numbers, which need not
        # equal the stat device for the same file (btrfs per-subvolume
        # anon_dev, pre-6.8 overlayfs). A maps deleted entry whose device is
        # in no slot output therefore proves nothing -- e.g. a replaced slot
        # proc-macro .so still mapped by a live process -- so it stays
        # fail-closed and refuses the whole run with no mutation.
        target = self.pool / 'slot-0' / 'target'
        observed = {os.stat(self.pool / f'slot-{n}' / sub).st_dev
                    for n in range(2) for sub in ('target', 'scratch')}
        foreign = os.makedev(0xAB, 0xCD)
        self.assertNotIn(foreign, observed)
        with patch.object(cache, 'process_references',
                          return_value=([], [('/other/tenant/stale.so', foreign, 999999991, False)])):
            with self.assertRaisesRegex(cache.Refusal, 'unresolved deleted'):
                self.retain()
        self.assertTrue((target / 'debug' / 'fixture').exists())
        self.assertTrue((self.pool / 'slot-0' / 'lease.json').exists())

    def test_non_file_maps_deleted_excluded_by_identity(self):
        # Shared-host shape: PostgreSQL backends map SYSV IPC segments and
        # /dev/zero, shown deleted on device 00:01. Those paths denote kernel
        # objects that can never be regular files, so they cannot alias slot
        # output and are excluded (and counted) by identity -- never by
        # device comparison. Real-path maps entries still refuse (above).
        shm = os.makedev(0x00, 0x01)
        with patch.object(cache, 'process_references',
                          return_value=([], [('/SYSV00000000', shm, 1, False),
                                             ('/dev/zero', shm, 2, False)])):
            receipt = self.retain()
        self.assertTrue(all(row['eligible'] for row in receipt['slots']))
        self.assertEqual(receipt['excluded_deleted_references'], 2)
        self.assertFalse((self.pool / 'slot-0' / 'lease.json').exists())

    def test_device_unknown_deleted_refuses_whole(self):
        # A deleted entry with no usable device identity cannot prove
        # non-aliasing, so it stays fail-closed and refuses the whole run.
        with patch.object(cache, 'process_references',
                          return_value=([], [('/elsewhere/stale.so', None, 7, True)])):
            with self.assertRaisesRegex(cache.Refusal, 'unresolved deleted'):
                self.retain()
        self.assertTrue((self.pool / 'slot-0' / 'lease.json').exists())

    def test_denied_process_scan_refuses_whole_without_mutation(self):
        self.fake_pid()
        with patch.object(os, 'readlink', side_effect=PermissionError('denied')):
            with self.assertRaisesRegex(cache.Refusal, 'visibility'):
                self.retain()
        self.assertTrue((self.pool / 'slot-0' / 'lease.json').exists())

    def test_unexpected_slot_contents_and_symlink_skip(self):
        slot = self.pool / 'slot-0'
        (slot / 'extra').mkdir()
        row = self.by_slot(self.retain())['slot-0']
        self.assertFalse(row['eligible'])
        self.assertIn('unexpected', row['reason'])
        shutil.rmtree(slot / 'extra')
        (slot / 'target' / 'debug' / 'link').symlink_to(self.root)
        row = self.by_slot(self.retain())['slot-0']
        self.assertFalse(row['eligible'])
        self.assertIn('incomplete slot scan', row['reason'])
        self.assertTrue((slot / 'lease.json').exists())
        (slot / 'target' / 'debug' / 'link').unlink()
        # A symlinked target directory itself must skip only that slot (with
        # a receipt for the healthy slot), never refuse the whole run: the
        # name check above passes while real_directory raises Refusal.
        shutil.rmtree(slot / 'target')
        (slot / 'target').symlink_to(self.root, target_is_directory=True)
        receipt = self.retain()
        by_slot = self.by_slot(receipt)
        self.assertFalse(by_slot['slot-0']['eligible'])
        self.assertIn('real slot directory', by_slot['slot-0']['reason'])
        self.assertTrue((slot / 'lease.json').exists())
        # The healthy slot is still processed (earlier retains in this test
        # already emptied it, so bytes need not move here).
        self.assertTrue(by_slot['slot-1']['eligible'])

    def test_external_tmp_is_never_touched(self):
        scratch = self.root / 'container-tmp'
        scratch.mkdir()
        for name in ('tog-10078-fixes-target', 'tog-10078-db-target', 'two-bot-next-s2'):
            target = scratch / name
            target.mkdir()
            (target / 'fixture').write_bytes(b'x' * 4096)
        receipt = self.retain()
        self.assertTrue(all(row['eligible'] for row in receipt['slots']))
        self.assertEqual(len(list(scratch.glob('*/fixture'))), 3)


if __name__ == '__main__':
    unittest.main()
