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
