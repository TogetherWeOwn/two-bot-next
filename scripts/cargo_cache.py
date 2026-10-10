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
