#!/usr/bin/env python3
"""TWO-only Cargo admission, read-only retention audit, and filesystem alarm.

No deletion, mounting, credential access, or service control. Linux + stdlib only.
A filesystem quota is the hard byte bound; the sampled guard is defense in depth.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import time

GIB = 1024 ** 3
DEFAULT_POOL = Path('/paperclip/.cache/two-bot-next-bounded')
TERMINAL = {'done', 'cancelled'}


class Refusal(Exception):
    pass


def real_directory(path):
    path = Path(os.path.abspath(path))
    # Reject symlinks anywhere in the path, not just at the leaf.
    if path.resolve() != path or not path.is_dir():
        raise Refusal(f'not a real directory: {path}')
    return path


def usage(path):
    """Allocated blocks, not logical lengths (sparse files do not fake usage)."""
    path = real_directory(path)
    total = 0
    seen = set()
    def unreadable(error):
        if not isinstance(error, FileNotFoundError):
            raise Refusal(f'incomplete cache scan: {error.filename}')

    for base, dirs, files in os.walk(path, followlinks=False, onerror=unreadable):
        for item in [Path(base)] + [Path(base) / name for name in files + dirs]:
            try:
                info = item.lstat()
            except FileNotFoundError:
                continue  # Cargo renamed/removed a temporary output during sampling.
            if stat.S_ISLNK(info.st_mode):
                raise Refusal(f'symlink in cache: {item}')
            key = (info.st_dev, info.st_ino)
            if key not in seen:
                seen.add(key)
                total += info.st_blocks * 512
    return total


def available(path):
    fs = os.statvfs(path)
    return fs.f_bavail * fs.f_frsize


def load_policy(pool):
    policy = json.loads((pool / 'policy.json').read_text())
    if policy.get('version') != 1:
        raise Refusal('unsupported pool policy')
    for field in ('slots', 'slot_budget_bytes', 'min_available_bytes', 'hard_limit_bytes'):
        if type(policy.get(field)) is not int or policy[field] <= 0:
            raise Refusal(f'invalid policy {field}')
    if policy['slots'] > 8:
        raise Refusal('more than 8 slots is not a bounded TWO pool')
    if policy['slots'] * policy['slot_budget_bytes'] > policy['hard_limit_bytes']:
        raise Refusal('slot budgets exceed the attested hard limit')
    if not isinstance(policy.get('quota_receipt'), str) or not policy['quota_receipt'].strip():
        raise Refusal('Operator quota receipt required; sampled checks are not a hard limit')
    if (not isinstance(policy.get('scratch_coverage_receipt'), str)
            or not policy['scratch_coverage_receipt'].strip()):
        raise Refusal('Operator scratch coverage receipt required; container /tmp is not covered by target audit')
    expected = {'policy.json'} | {f'slot-{n}' for n in range(policy['slots'])}
    if {p.name for p in pool.iterdir()} != expected:
        raise Refusal('pool contents do not match immutable policy')
    return policy


def acquire(pool, policy):
    for number in range(policy['slots']):
        slot = real_directory(pool / f'slot-{number}')
        if {p.name for p in slot.iterdir()} - {'lock', 'target', 'scratch', 'lease.json'}:
            raise Refusal(f'unexpected slot contents: {slot}')
        target = real_directory(slot / 'target')
        real_directory(slot / 'scratch')
        fd = os.open(slot / 'lock', os.O_RDWR | os.O_NOFOLLOW)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            os.close(fd)
            continue
        try:
            if (slot / 'lease.json').exists():
                # The slot flock is the liveness proof: it is passed to Cargo
                # through pass_fds, so holding it means no wrapper, Cargo, or
                # fd-inheriting descendant is alive. A stale under-budget
                # lease whose recorded Cargo process group is also dead is
                # recovered here; anything else still needs the Operator.
                try:
                    lease = json.loads((slot / 'lease.json').read_text())
                except (OSError, ValueError):
                    lease = None
                pgid = lease.get('cargo_pgid') if isinstance(lease, dict) else None
                if (isinstance(pgid, int) and not isinstance(pgid, bool) and pgid > 0
                        and usage(slot) < policy['slot_budget_bytes']
                        and not group_alive(pgid)):
                    print(json.dumps({'finding': 'stale_lease_recovered',
                                      'slot': slot.name, **lease}, sort_keys=True),
                          file=sys.stderr)
                    (slot / 'lease.json').unlink()
                    return slot, target, fd
                os.close(fd)
                continue  # Interrupted/oversized/live run: Operator must inspect it.
            if usage(slot) >= policy['slot_budget_bytes']:
                os.close(fd)
                continue  # Oversized retained output: Operator must inspect it.
            return slot, target, fd
        except Exception:
            os.close(fd)
            raise
    raise Refusal('no idle, below-budget slot; no per-worktree fallback')


def validate_cargo_args(args, workspace):
    if not args or args[0] not in {'build', 'check', 'test', 'clippy', 'fmt'}:
        raise Refusal('supported Cargo commands: build, check, test, clippy, fmt')
    for number, arg in enumerate(args):
        if any(arg == flag or arg.startswith(flag + '=') for flag in (
                '--target-dir', '--build-dir', '--config', '--artifact-dir', '--out-dir', '-Z')) or arg.startswith('-Z'):
            raise Refusal('Cargo output/config overrides are not allowed')
        if arg == '--manifest-path' or arg.startswith('--manifest-path='):
            value = arg.split('=', 1)[1] if '=' in arg else args[number + 1]
            if not (workspace / value).resolve().is_relative_to(workspace):
                raise Refusal('manifest must stay in this workspace')


def resolve_cargo(cargo):
    """Executable Cargo path, resolved before any pool access.

    Agent PATH may lack rustup's bin directory, so a bare name falls back to
    $CARGO_HOME/bin, then ~/.cargo/bin. A miss is a Refusal: a spawn that
    cannot exec must never be able to leave a lease behind.
    """
    found = shutil.which(cargo)
    if found:
        return found
    if os.sep not in cargo:
        for home in (os.environ.get('CARGO_HOME'), os.path.expanduser('~/.cargo')):
            if home:
                candidate = os.path.join(home, 'bin', cargo)
                if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
                    return candidate
    raise Refusal(f'{cargo} not found on PATH, in $CARGO_HOME/bin or ~/.cargo/bin; '
                  'no lease taken')


def group_alive(pid):
    try:
        os.killpg(pid, 0)
        return True
    except ProcessLookupError:
        return False


def stop_group(child):
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(child.pid, sig)
        except ProcessLookupError:
            break
        if sig == signal.SIGTERM:
            try:
                child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
    child.wait()


def run_cargo(pool, args, cargo='cargo', interval=1, _before_stop=None):
    # _before_stop is a deterministic transition seam for the offline suite:
    # it runs inside cleanup after cancellation starts but before termination
    # handlers are ignored, reproducing the exact window a second signal can
    # land in. Production callers leave it None.
    pool = real_directory(pool)
    workspace = real_directory(Path.cwd())
    validate_cargo_args(args, workspace)
    cargo = resolve_cargo(cargo)
    if args[0] != 'fmt':
        cargo_flags = args[:args.index('--')] if '--' in args else args
        required = [flag for flag in ('--offline', '--locked') if flag not in cargo_flags]
        args = [args[0]] + required + args[1:]
    policy = load_policy(pool)
    if available(pool) < policy['min_available_bytes']:
        raise Refusal('backing filesystem has insufficient user-available bytes')
    slot, target, fd = acquire(pool, policy)
    child = None
    previous = {}
    clean_exit = False
    spawn_failed = False
    cancelling = False
    spawning = True
    pending_signal = None

    def interrupted(signum, frame):
        # Cancellation is armed before spawn. A signal landing inside Popen
        # or before the child handle exists must neither raise out of spawn
        # internals (which would leave child=None and bypass cleanup) nor
        # fall through to the default handler (which would kill the wrapper
        # without stopping the group). Record it instead; the wrapper stops
        # the group on the pending cancellation immediately after spawn.
        nonlocal cancelling, pending_signal
        if cancelling:
            return  # a later signal during cleanup must not raise again
        cancelling = True
        if spawning or child is None:
            pending_signal = signum
            return
        raise Refusal(f'build interrupted by signal {signum}; lease retained')

    try:
        for sig in (signal.SIGINT, signal.SIGTERM):
            previous[sig] = signal.signal(sig, interrupted)
        # Exclusive create + persistent sentinel protect against wrapper death.
        with (slot / 'lease.json').open('x') as output:
            json.dump({'workspace': str(workspace), 'wrapper_pid': os.getpid(),
                       'started_at': time.time()}, output)
        env = os.environ.copy()
        # Repository tests prefer PAPERCLIP_RUN_SCRATCH_DIR over temp_dir
        # (crates/core/src/mac.rs), so repository scratch selectors must also
        # point at lease scratch. Child-only overrides; the parent is unchanged.
        env.update(CARGO_TARGET_DIR=str(target), CARGO_BUILD_TARGET_DIR=str(target),
                   CARGO_BUILD_BUILD_DIR=str(target),
                   TMPDIR=str(slot / 'scratch'), TMP=str(slot / 'scratch'),
                   TEMP=str(slot / 'scratch'),
                   PAPERCLIP_RUN_SCRATCH_DIR=str(slot / 'scratch'),
                   PAPERCLIP_SCRATCH_DIR=str(slot / 'scratch'),
                   CARGO_INCREMENTAL='0', CARGO_PROFILE_DEV_DEBUG='0',
                   CARGO_PROFILE_TEST_DEBUG='0')
        # A fallback-resolved rustup proxy needs its sibling proxies (rustc,
        # cargo-fmt, clippy-driver) on the child PATH as well.
        bindir = os.path.dirname(cargo)
        if bindir not in env.get('PATH', '').split(os.pathsep):
            env['PATH'] = os.pathsep.join(filter(None, (bindir, env.get('PATH'))))
        # Pass the lease FD to Cargo as well: wrapper SIGKILL must not free it.
        try:
            child = subprocess.Popen([cargo] + args, env=env, start_new_session=True,
                                     pass_fds=(fd,))
        except OSError:
            # Exec/fork failed: Popen reaped any forked child, so no writer
            # ever existed, and our flock still guards the slot. Release our
            # own sentinel instead of wedging the slot for the Operator.
            spawn_failed = True
            raise
        # Record Cargo's process group (start_new_session: pgid == child pid)
        # so a later acquire can tell a dead group from an escaped descendant
        # that closed the fd. Atomic rewrite; the temp file lives in scratch
        # so a crash can never leave an unexpected top-level slot entry.
        tmp_path = slot / 'scratch' / 'lease.json.tmp'
        with tmp_path.open('w') as output:
            json.dump({**json.loads((slot / 'lease.json').read_text()),
                       'cargo_pgid': child.pid}, output)
        os.replace(tmp_path, slot / 'lease.json')
        spawning = False
        if pending_signal is not None:
            raise Refusal(f'build interrupted by signal {pending_signal}; lease retained')
        while True:
            if usage(slot) >= policy['slot_budget_bytes']:
                raise Refusal('slot reached sampled byte budget; lease retained')
            if available(pool) < policy['min_available_bytes']:
                raise Refusal('filesystem available-byte floor reached; lease retained')
            result = child.poll()
            if result is not None:
                if group_alive(child.pid):
                    raise Refusal('descendant process outlived Cargo; lease retained')
                clean_exit = True
                return result
            time.sleep(interval)
    finally:
        try:
            if child is not None and not clean_exit:
                # Cancellation is idempotent from the first signal onward:
                # interrupted() sets cancelling before raising, and every
                # later signal is a no-op, so no signal can raise out of this
                # cleanup and stop_group always runs. Mark it here too so a
                # non-signal Refusal (budget/floor) gets the same protection.
                cancelling = True
                if _before_stop is not None:
                    _before_stop()
                for sig in (signal.SIGINT, signal.SIGTERM):
                    signal.signal(sig, signal.SIG_IGN)
                stop_group(child)
        finally:
            for sig, handler in previous.items():
                signal.signal(sig, handler)
            if clean_exit or spawn_failed:
                (slot / 'lease.json').unlink()
            os.close(fd)


def PART2():
    pass
