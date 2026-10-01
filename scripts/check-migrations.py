"""Check bot migration numbering and the reviewed, byte-for-byte checksum lock.

No database access or third-party Python packages. See docs/migrations.md.
"""

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
PATH = re.compile(r"crates/[^/]+/migrations/([0-9]{4})_[a-z][a-z0-9_]*\.sql")
CHECKSUM = re.compile(r"[0-9a-f]{64}")


class MigrationError(ValueError):
    pass


def digest(contents):
    return hashlib.sha256(contents).hexdigest()


def migration_path(path):
    parts = PurePosixPath(path).parts
    return len(parts) >= 4 and parts[0] == "crates" and parts[2] == "migrations"


def read_migrations(root):
    migrations = {}
    crates = root / "crates"
    if crates.is_symlink() or any(crate.is_symlink() for crate in crates.glob("*")):
        raise MigrationError(f"{crates}: migration ancestors cannot be symlinks")
    for directory in sorted(crates.glob("*/migrations")):
        if directory.is_symlink() or directory.parent.is_symlink():
            raise MigrationError(f"{directory}: migration directories cannot be symlinks")
        for file in sorted(directory.rglob("*")):
            if file.is_symlink():
                raise MigrationError(f"{file}: migrations cannot be symlinks")
            if file.is_file():
                migrations[file.relative_to(root).as_posix()] = digest(file.read_bytes())
    return migrations


def read_lock(contents):
    try:
        lock = json.loads(contents, object_pairs_hook=unique_keys)
    except (ValueError, UnicodeError) as error:
        raise MigrationError(f"migrations.lock: {error}") from error
    if not isinstance(lock, dict) or set(lock) != {"version", "migrations"} or type(lock["version"]) is not int or lock["version"] != 1:
        raise MigrationError("migrations.lock: expected version 1 and migrations object")
    entries = lock["migrations"]
    if not isinstance(entries, dict):
        raise MigrationError("migrations.lock: migrations must be an object")
    for path, entry in entries.items():
        if not PATH.fullmatch(path):
            raise MigrationError(f"migrations.lock: invalid migration path {path!r}")
        if not isinstance(entry, dict) or set(entry) != {"sha256", "justification"}:
            raise MigrationError(f"{path}: lock entry needs sha256 and justification")
        if not isinstance(entry["sha256"], str) or not CHECKSUM.fullmatch(entry["sha256"]):
            raise MigrationError(f"{path}: invalid SHA-256")
        reason = entry["justification"]
        if not isinstance(reason, str) or not reason.strip() or len(reason.splitlines()) != 1:
            raise MigrationError(f"{path}: justification must be one nonempty line")
    return entries


def unique_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise MigrationError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def git(root, *args):
    result = subprocess.run(["git", "-C", str(root), *args], capture_output=True)
    if result.returncode:
        raise MigrationError(f"cannot read Git baseline: {result.stderr.decode(errors='replace').strip()}")
    return result.stdout


def baseline(root, ref):
    # Resolve once, fail closed on a missing/shallow/invalid revision. Blob reads
    # use the resolved object ID, never an unchecked ref in a shell command.
    revision = git(root, "rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}").decode().strip()
    paths = []
    for record in git(root, "ls-tree", "-r", "-z", revision).decode().split("\0"):
        if not record:
            continue
        metadata, path = record.split("\t", 1)
        parts = PurePosixPath(path).parts
        if metadata.split()[0] == "120000" and (
            path == "migrations.lock" or (parts[0] == "crates" and (len(parts) <= 2 or parts[2] == "migrations"))
        ):
            raise MigrationError(f"{path}: baseline lock, migrations and ancestors cannot be symlinks")
        paths.append(path)
    migrations = {path: digest(git(root, "show", f"{revision}:{path}")) for path in paths if migration_path(path)}
    lock = read_lock(git(root, "show", f"{revision}:migrations.lock")) if "migrations.lock" in paths else {}
    return migrations, lock


def validate(migrations, lock, previous=None, previous_lock=None):
    numbers = {}
    for path in sorted(migrations):
        match = PATH.fullmatch(path)
        if not match:
            raise MigrationError(f"{path}: expected crates/*/migrations/NNNN_name.sql")
        number = int(match[1])
        if not 1 <= number <= 999:
            raise MigrationError(f"{path}: number must be in bot range 0001-0999")
        if number in numbers:
            raise MigrationError(f"duplicate migration number {number:04d}: {numbers[number]}, {path}")
        numbers[number] = path
    if set(migrations) != set(lock):
        missing = sorted(set(migrations) - set(lock))
        stale = sorted(set(lock) - set(migrations))
        raise MigrationError(f"migrations.lock path mismatch: unlocked={missing}, missing files={stale}")
    for path, checksum in migrations.items():
        if checksum != lock[path]["sha256"]:
            raise MigrationError(f"{path}: checksum changed; use a new migration or intentionally update the lock with a justification")
    for path, checksum in (previous or {}).items():
        if path not in migrations:
            raise MigrationError(f"{path}: existing migrations cannot be removed or renamed")
        if migrations[path] != checksum:
            old_reason = (previous_lock or {}).get(path, {}).get("justification", "")
            if lock[path]["justification"].split() == old_reason.split():
                raise MigrationError(f"{path}: changed migration needs a fresh justification line relative to the Git baseline")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help="workspace root (default: script's repository)")
    parser.add_argument("--base-ref", help="Git baseline for detecting lock rewrites, deletions and renames")
    args = parser.parse_args(argv)
    try:
        migrations = read_migrations(args.root)
        lock_path = args.root / "migrations.lock"
        if lock_path.is_symlink():
            raise MigrationError(f"{lock_path}: migrations.lock cannot be a symlink")
        lock = read_lock(lock_path.read_bytes())
        previous, previous_lock = baseline(args.root, args.base_ref) if args.base_ref else (None, None)
        validate(migrations, lock, previous, previous_lock)
    except (MigrationError, OSError) as error:
        print(f"Migration check failed: {error}", file=sys.stderr)
        return 1
    print(f"Migration check OK: {len(migrations)} locked bot migrations" + (f"; baseline {args.base_ref}" if args.base_ref else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
