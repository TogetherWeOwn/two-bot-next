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


def process_references(proc_root):
    """Host PID namespace, including container processes. Denied reads fail closed.

    Returns (references, deleted): live path references plus unlinked/replaced
    artifact references kept with their deletion marker preserved. A deleted
    entry under a container spelling cannot be mapped to a host path by
    lexical match, and its old inode may be gone from the workspace
    traversal, so callers must never silently treat it as no reference.

    Scans cwd/exe/fd/maps plus cmdline argv tokens. Inodes are compared too,
    so container mount path spellings need not match the host. cmdline entries
    that no longer stat are kept as lexical-only references (dev/ino None);
    any denied read aborts the whole scan.
    """
    references = []
    deleted = []
    for entry in Path(proc_root).iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            fields = (entry / 'stat').read_text().rsplit(')', 1)[1].split()
            # Kernel threads have no userspace cwd/exe/fd/maps; skip them.
            if int(fields[6]) & 0x00200000:
                continue
            # A zombie leader is not proof its thread group is dead: a
            # multithreaded process whose main thread exited can keep live
            # workers holding workspace references. Skip a zombie only with
            # positive evidence of a single remaining task; an ambiguous or
            # multithreaded zombie group fails the whole audit closed.
            if fields[0] == 'Z':
                try:
                    tasks = [task for task in (entry / 'task').iterdir()
                             if task.name.isdecimal()]
                except FileNotFoundError:
                    if entry.exists():
                        raise Refusal(f'incomplete process visibility: pid {entry.name}')
                    continue  # exited between stat and task listing; no references
                except PermissionError:
                    raise Refusal(f'incomplete process visibility: pid {entry.name}')
                if len(tasks) != 1:
                    raise Refusal(
                        f'incomplete process visibility: zombie pid {entry.name} '
                        f'has {len(tasks)} tasks')
                continue
            for link in [entry / 'cwd', entry / 'exe'] + list((entry / 'fd').iterdir()):
                try:
                    name = os.readlink(link)
                    info = link.stat()
                except FileNotFoundError:
                    # Exit/closed-FD race is okay only if the process or FD is gone.
                    if not entry.exists() or (link.parent.name == 'fd' and not link.exists()):
                        continue
                    raise
                if name.endswith(' (deleted)'):
                    deleted.append((name.removesuffix(' (deleted)'),
                                    info.st_dev, info.st_ino))
                elif name.startswith('/'):
                    references.append((name, info.st_dev, info.st_ino))
            for line in (entry / 'maps').read_text().splitlines():
                fields = line.split(None, 5)
                if len(fields) == 6 and fields[5].startswith('/'):
                    if fields[5].endswith(' (deleted)'):
                        major, minor = fields[3].split(':')
                        deleted.append((fields[5].removesuffix(' (deleted)'),
                                        os.makedev(int(major, 16), int(minor, 16)),
                                        int(fields[4])))
                    else:
                        major, minor = fields[3].split(':')
                        references.append((fields[5],
                                           os.makedev(int(major, 16), int(minor, 16)),
                                           int(fields[4])))
            # cmdline argv tokens: a process started with an absolute slot or
            # target path (e.g. cargo --target-dir, a shell cd'd into output)
            # is a live reference even when cwd/exe/fd/maps show nothing.
            # Fixture proc roots may omit cmdline; treat a missing file as no
            # cmdline references. Any denied read fails the whole scan closed.
            try:
                raw = (entry / 'cmdline').read_bytes()
            except FileNotFoundError:
                if entry.exists() and (entry / 'cmdline').exists():
                    raise Refusal(f'incomplete process visibility: pid {entry.name}')
            except PermissionError:
                raise Refusal(f'incomplete process visibility: pid {entry.name}')
            else:
                for token in raw.split(b'\0'):
                    if not token.startswith(b'/'):
                        continue
                    try:
                        text = os.fsdecode(token)
                    except (UnicodeDecodeError, ValueError):
                        continue
                    try:
                        info = os.stat(text)
                    except FileNotFoundError:
                        references.append((text, None, None))
                    except NotADirectoryError:
                        references.append((text, None, None))
                    except (PermissionError, OSError):
                        raise Refusal(f'incomplete process visibility: pid {entry.name}')
                    else:
                        references.append((text, info.st_dev, info.st_ino))
        except FileNotFoundError:
            if entry.exists():
                raise Refusal(f'incomplete process visibility: pid {entry.name}')
        except PermissionError:
            raise Refusal(f'incomplete process visibility: pid {entry.name}')
    return references, deleted


def within(path, root):
    return Path(path).is_relative_to(root)


# Top-level entries Cargo itself creates in a target directory. A .gitignore
# entry proves nothing about provenance, so any other top-level entry is
# foreign material and vetoes retention eligibility.
CARGO_TARGET_TOP_LEVEL = frozenset(
    {'debug', 'release', 'doc', 'package', 'tmp',
     '.rustc_info.json', '.cargo-lock', 'CACHEDIR.TAG'})
# File kinds that must never be treated as regenerable build output.
PROTECTED_SUFFIXES = frozenset(
    {'.rs', '.md', '.markdown', '.toml', '.bak', '.orig', '.rej', '.pem', '.key'})
# Directory names that suggest preserved evidence/backups/archives.
PROTECTED_DIR_NAMES = frozenset(
    {'evidence', 'backup', 'backups', 'archive', 'archives',
     'incident', 'incidents', 'receipt', 'receipts'})


def preservation_veto(target):
    """Reason a target's contents veto retention, or None when build-output-only.

    Calibrated against real Cargo output: a genuine target holds only
    debug/release/doc/package/tmp trees at top level. Nested build-script
    codegen (debug/build/*/out/*.rs) is expected; .rs anywhere else is not
    Cargo output. Unknown foreign top-level entries fail closed to a veto.
    """
    def unreadable(error):
        raise Refusal(f'incomplete target scan: {error.filename}')

    first = True
    for base, dirs, files in os.walk(target, followlinks=False, onerror=unreadable):
        if first:
            for name in dirs + files:
                if name not in CARGO_TARGET_TOP_LEVEL:
                    return f'preserved foreign top-level entry: {name}'
            first = False
        under_codegen_out = (
            Path(base).name == 'out'
            and 'build' in Path(base).relative_to(target).parts)
        for name in dirs:
            if name.lower() in PROTECTED_DIR_NAMES:
                return f'preserved directory in target: {name}'
        for name in files:
            lowered = name.lower()
            stem = lowered
            while '.' in stem:
                stem = stem.rsplit('.', 1)[0]
            if stem in PROTECTED_DIR_NAMES:
                return f'preserved file in target: {name}'
            suffix = Path(lowered).suffix
            if suffix in PROTECTED_SUFFIXES:
                if suffix == '.rs' and under_codegen_out:
                    continue  # build-script codegen output, not stashed source
                return f'preserved file in target: {name}'
    return None


def workspace_inodes(workspace):
    # Includes source/evidence, so a process working anywhere in the workspace
    # vetoes retention, even when container path spellings differ from the host.
    result = set()

    def unreadable(error):
        raise Refusal(f'incomplete workspace scan: {error.filename}')

    for base, dirs, files in os.walk(workspace, followlinks=False, onerror=unreadable):
        for path in [Path(base)] + [Path(base) / name for name in dirs + files]:
            info = path.lstat()
            result.add((info.st_dev, info.st_ino))
    return result


def _veto_scan_names(base, target, dirs, files, check_top_level, first):
    """Shared protected-name veto used by target and scratch scans."""
    if first and check_top_level:
        for name in dirs + files:
            if name not in CARGO_TARGET_TOP_LEVEL:
                return f'preserved foreign top-level entry: {name}'
    under_codegen_out = (
        Path(base).name == 'out'
        and 'build' in Path(base).relative_to(target).parts)
    for name in dirs:
        if name.lower() in PROTECTED_DIR_NAMES:
            return f'preserved directory in target: {name}'
    for name in files:
        lowered = name.lower()
        stem = lowered
        while '.' in stem:
            stem = stem.rsplit('.', 1)[0]
        if stem in PROTECTED_DIR_NAMES:
            return f'preserved file in target: {name}'
        suffix = Path(lowered).suffix
        if suffix in PROTECTED_SUFFIXES:
            if suffix == '.rs' and under_codegen_out:
                continue  # build-script codegen output, not stashed source
            return f'preserved file in target: {name}'
    return None


def scratch_preservation_veto(scratch):
    """Reason a slot scratch tree vetoes retention, or None.

    Scratch holds cooperative compiler/test temp files with arbitrary names,
    so unlike target there is no Cargo-defined top-level allowlist: any
    non-protected entry is treated as regenerable temp output. Protected
    directory names, protected file stems/suffixes, foreign symlinks and any
    other top-level foreign material still veto. Heuristics are a backstop
    veto only; eligibility still needs the attested classification.
    """
    def unreadable(error):
        raise Refusal(f'incomplete scratch scan: {error.filename}')

    for base, dirs, files in os.walk(scratch, followlinks=False, onerror=unreadable):
        for name in dirs + files:
            try:
                if (Path(base) / name).is_symlink():
                    return f'symlink in scratch: {name}'
            except OSError:
                raise Refusal(f'incomplete scratch scan: {name}')
        veto = _veto_scan_names(base, scratch, dirs, files,
                                check_top_level=False, first=False)
        if veto is not None:
            return veto.replace('in target', 'in scratch')
    return None


def slot_output_inodes(target, scratch):
    """(dev, ino) identities for every entry under target+scratch.

    Symlinks are reported as a veto string instead of being followed, so the
    caller skips the slot instead of measuring or deleting through a link.
    Returns (veto_or_None, nodes).
    """
    nodes = set()

    def unreadable(error):
        raise Refusal(f'incomplete slot scan: {error.filename}')

    for root in (target, scratch):
        for base, dirs, files in os.walk(root, followlinks=False, onerror=unreadable):
            for name in [None] + dirs + files:
                path = Path(base) if name is None else Path(base) / name
                try:
                    info = path.lstat()
                except FileNotFoundError:
                    continue  # Cargo renamed output mid-scan; re-verify aborts below
                if stat.S_ISLNK(info.st_mode):
                    return f'symlink in slot output: {path}', set()
                nodes.add((info.st_dev, info.st_ino))
    return None, nodes


def validate_shared_pool_inventory(pool, policy, inventory, now, max_age):
    """Validate a shared-pool retain inventory; refuse the whole run on ambiguity.

    Expected shape (caller-supplied, host-scope, fresh):
      {"version": 1, "complete": True, "process_scope": "host",
       "captured_at_unix": <unix>,
       "slots": [{"path": "<pool>/slot-N", "issue_id": "<uuid>",
                  "status": "done|...", "live_run": bool, "referenced": bool,
                  "target_provenance": "build_output_only"}, ...]}
    live_run covers running/queued/retry runs; referenced covers all
    execution/project/shared workspace refs. Returns (indexed, captured).
    """
    if (inventory.get('version') != 1 or inventory.get('complete') is not True
            or inventory.get('process_scope') != 'host'):
        raise Refusal('need complete host-scope control-plane inventory')
    captured = inventory.get('captured_at_unix')
    if type(captured) not in (int, float) or not 0 <= now - captured <= max_age:
        raise Refusal('control-plane inventory is stale or from the future')
    rows = inventory.get('slots')
    if not isinstance(rows, list):
        raise Refusal('inventory slots must be a list')
    indexed = {}
    identities = {}
    target_identities = {}
    for row in rows:
        raw = row['path']
        if not isinstance(raw, str) or not os.path.isabs(raw):
            raise Refusal('ambiguous slot attribution')
        canonical = os.path.normpath(raw)
        if raw != canonical or canonical in indexed:
            raise Refusal('ambiguous slot attribution')
        if type(row.get('live_run')) is not bool or type(row.get('referenced')) is not bool:
            raise Refusal('ambiguous slot attribution')
        if not row.get('issue_id') or row.get('status') not in TERMINAL | {
                'backlog', 'todo', 'in_progress', 'in_review', 'blocked'}:
            raise Refusal('missing slot attribution/status')
        try:
            resolved = os.path.realpath(canonical)
        except OSError:
            raise Refusal('ambiguous slot attribution')
        if resolved != canonical:
            raise Refusal('ambiguous slot attribution')
        try:
            info = os.stat(canonical)
        except FileNotFoundError:
            identity = None  # dangling row; covers nothing, matches nothing
        except (PermissionError, OSError):
            raise Refusal(f'incomplete slot visibility: {canonical}')
        else:
            if not stat.S_ISDIR(info.st_mode):
                raise Refusal('ambiguous slot attribution')
            identity = (info.st_dev, info.st_ino)
            if identity in identities:
                raise Refusal('ambiguous slot attribution')
            identities[identity] = canonical
        try:
            target_info = os.stat(canonical + '/target')
        except FileNotFoundError:
            target_identity = None
        except (PermissionError, OSError):
            raise Refusal(f'incomplete slot visibility: {canonical}/target')
        else:
            target_identity = (target_info.st_dev, target_info.st_ino)
            if target_identity in target_identities:
                raise Refusal('ambiguous slot target attribution')
            target_identities[target_identity] = canonical
        indexed[canonical] = row
    return indexed, captured


def _lock_identity(fd):
    info = os.fstat(fd)
    return (info.st_dev, info.st_ino)


def _lock_path_identity(lock_path):
    info = os.stat(lock_path)
    return (info.st_dev, info.st_ino)


def shared_pool_retain(pool, inventory, proc_root='/proc', now=None, max_age=60,
                       _before_mutation=None):
    """Reclaim over-budget shared-pool slots with lock held through mutation.

    Distinct from the legacy-worktree audit: exact slot paths only, never
    /tmp globbing, registries, quotas or policies. For each policy slot the
    SAME lock fd/inode is held from re-verify through deletion and
    recreation; the lock file is never replaced and policy.json is never
    touched. Any doubt skips that slot and keeps its lease. Global ambiguity
    (bad inventory, unreadable proc scan, replaced pool layout) refuses the
    whole run with no mutation. Returns a JSON-serializable receipt.
    """
    pool = real_directory(pool)
    policy = load_policy(pool)
    started = time.monotonic()
    now = time.time() if now is None else now
    indexed, captured = validate_shared_pool_inventory(pool, policy, inventory, now, max_age)
    # Phase 1: acquire every available slot lock non-blocking and hold the
    # fds. Unacquirable slots skip (keep lease); nothing is mutated yet.
    held = {}
    results = {}
    for number in range(policy['slots']):
        name = f'slot-{number}'
        slot = pool / name
        try:
            fd = os.open(slot / 'lock', os.O_RDWR | os.O_NOFOLLOW)
        except OSError:
            results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                             'reason': 'lock open failed; skipped, lease kept'}
            continue
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            os.close(fd)
            results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                             'reason': 'lock held by another process; skipped, lease kept'}
            continue
        try:
            if _lock_path_identity(slot / 'lock') != _lock_identity(fd):
                os.close(fd)
                results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                 'reason': 'lock file replaced; skipped, lease kept'}
                continue
        except OSError:
            os.close(fd)
            results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                             'reason': 'lock stat failed; skipped, lease kept'}
            continue
        held[name] = (slot, fd)
    try:
        if not 0 <= now - captured <= max_age:
            raise Refusal('control-plane inventory is stale or from the future')
        # Single root-visible process scan while ALL acquired locks are held,
        # so no new wrapper/Cargo writer can start mid-scan for a held slot.
        refs, deleted = process_references(proc_root)
        # Per-slot re-verify while holding that slot's lock. No mutation yet,
        # so the unresolved-deleted fail-closed check below runs before any
        # deletion.
        pending = {}
        for name in sorted(held):
            slot, _fd = held[name]
            try:
                if {p.name for p in slot.iterdir()} - {'lock', 'target', 'scratch', 'lease.json'}:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': 'unexpected slot contents; skipped, lease kept'}
                    continue
                try:
                    target = real_directory(slot / 'target')
                    scratch = real_directory(slot / 'scratch')
                except Refusal as error:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': f'not a real slot directory ({error}); skipped, lease kept'}
                    continue
                try:
                    before_bytes = usage(slot)
                except Refusal as error:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': f'incomplete slot scan ({error}); skipped, lease kept'}
                    continue
                row = indexed.get(str(slot))
                if row is None:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': 'unattributed slot; skipped, lease kept'}
                    continue
                if row['status'] not in TERMINAL or row['live_run'] or row['referenced']:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': ('live run, slot reference, or nonterminal issue; '
                                                'skipped, lease kept')}
                    continue
                if row.get('target_provenance') != 'build_output_only':
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': ('unclassified or mixed slot provenance; Operator '
                                                'build-output-only classification required; skipped')}
                    continue
                try:
                    veto = preservation_veto(target)
                except Refusal as error:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': f'incomplete target scan ({error}); skipped'}
                    continue
                if veto is not None:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': f'{veto}; skipped, lease kept'}
                    continue
                try:
                    veto = scratch_preservation_veto(scratch)
                except Refusal as error:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': f'incomplete scratch scan ({error}); skipped'}
                    continue
                if veto is not None:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': f'{veto}; skipped, lease kept'}
                    continue
                try:
                    link_veto, nodes = slot_output_inodes(target, scratch)
                except Refusal as error:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': f'incomplete slot scan ({error}); skipped'}
                    continue
                if link_veto is not None:
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': f'{link_veto}; skipped, lease kept'}
                    continue
                if any(within(path, target) or within(path, scratch)
                       or (device is not None and (device, inode) in nodes)
                       for path, device, inode in refs):
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': ('actual process cwd/exe/fd/map/cmdline reference; '
                                                'skipped, lease kept')}
                    continue
                if any(within(path, target) or within(path, scratch)
                       or (device is not None and (device, inode) in nodes)
                       for path, device, inode in deleted):
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': ('actual process reference (deleted artifact); '
                                                'skipped, lease kept')}
                    continue
                pending[name] = {'slot': slot, 'target': target, 'scratch': scratch,
                                 'nodes': nodes, 'before_bytes': before_bytes}
            except (OSError, ValueError, KeyError, IndexError, subprocess.CalledProcessError) as error:
                results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                 'reason': f'verify failed ({error}); skipped, lease kept'}
        # Fail-closed unresolved-deleted check before ANY mutation. A deleted
        # (unlinked/replaced) artifact keeps its deletion marker: under a
        # container spelling it matches neither a host path nor a live slot
        # inode, so an entry attributable to no held slot refuses the whole
        # run -- EXCEPT provable different-filesystem exclusion. One
        # filesystem's unlinked inode can never be another filesystem's file,
        # so a deleted entry whose device appears in no held slot output is
        # provably unable to reference slot output and is excluded (counted
        # in the receipt, never silently dropped). Same-filesystem and
        # device-unknown unattributed entries still refuse: a deleted slot
        # file held open carries the slot's device with an inode already gone
        # from the traversal, which no scan can distinguish from an unrelated
        # same-filesystem temp file. Exclusion also needs complete device
        # knowledge: if any held slot's output device is unreadable, nothing
        # is excluded and the strict rule applies.
        all_nodes = set()
        for entry in pending.values():
            all_nodes |= entry['nodes']
        slot_roots = [str((held[name][0] / 'target')) for name in held]
        slot_roots += [str((held[name][0] / 'scratch')) for name in held]
        for name in sorted(set(results) - set(held)):
            # Skipped (lock-held) slots still own lexical paths; an entry
            # inside one attributes there and must not force a whole refusal.
            slot = pool / name
            slot_roots += [str(slot / 'target'), str(slot / 'scratch')]
        slot_devices = {device for device, _inode in all_nodes}
        devices_complete = True
        for name in held:
            for sub in ('target', 'scratch'):
                try:
                    slot_devices.add(os.stat(held[name][0] / sub).st_dev)
                except OSError:
                    # That slot is already skipped; without its device the
                    # different-filesystem proof is unsound, so exclude nothing.
                    devices_complete = False
        unresolved = []
        excluded_deleted = 0
        for path, device, inode in deleted:
            if any(within(path, Path(root)) for root in slot_roots):
                continue  # lexical attribution, including skipped-slot paths
            if device is not None and (device, inode) in all_nodes:
                continue  # inode attribution to pending slot output
            if (devices_complete and device is not None
                    and device not in slot_devices):
                excluded_deleted += 1
                continue
            unresolved.append((path, device, inode))
        if unresolved:
            raise Refusal('unresolved deleted process reference; '
                          'exact-path process-reference receipt required')
        if now - captured + time.monotonic() - started > max_age:
            raise Refusal('retain took too long; capture a new inventory')
        # Phase 2: exact-path mutation per eligible slot, lock still held.
        ordered = []
        for name in sorted(pending):
            slot = pending[name]['slot']
            target = pending[name]['target']
            scratch = pending[name]['scratch']
            before_bytes = pending[name]['before_bytes']
            _slot, fd = held[name]
            try:
                if _lock_path_identity(slot / 'lock') != _lock_identity(fd):
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': 'lock file replaced before mutation; skipped'}
                    continue
                if _before_mutation is not None:
                    _before_mutation(slot, fd)
                    if _lock_path_identity(slot / 'lock') != _lock_identity(fd):
                        results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                         'reason': 'lock file replaced; skipped, lease kept'}
                        continue
                # Exact paths only: slot-N/target, slot-N/scratch,
                # slot-N/lease.json. Lock inode and policy.json are preserved.
                if not target.is_relative_to(slot) or not scratch.is_relative_to(slot):
                    results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                     'reason': 'slot path escape; skipped, lease kept'}
                    continue
                shutil.rmtree(target)
                shutil.rmtree(scratch)
                lease = slot / 'lease.json'
                try:
                    lease.unlink()
                except FileNotFoundError:
                    pass
                target.mkdir()
                scratch.mkdir()
                if _lock_path_identity(slot / 'lock') != _lock_identity(fd):
                    raise Refusal(f'lock file replaced during mutation: {name}')
                after_bytes = usage(slot)
                results[name] = {'slot': name, 'path': str(slot), 'eligible': True,
                                 'reason': 'retained; exact slot output reclaimed',
                                 'before_bytes': before_bytes, 'after_bytes': after_bytes,
                                 'reclaimed_bytes': before_bytes - after_bytes,
                                 'lock_held_through_mutation': True}
            except Refusal:
                raise
            except (OSError, ValueError) as error:
                results[name] = {'slot': name, 'path': str(slot), 'eligible': False,
                                 'reason': f'mutation failed ({error}); skipped'}
        for name in sorted(results):
            ordered.append(results[name])
        return {'version': 1, 'retain': True, 'mutation': True,
                'captured_at_unix': now,
                'excluded_deleted_references': excluded_deleted,
                'slots': ordered}
    finally:
        for _slot, fd in held.values():
            try:
                os.close(fd)
            except OSError:
                pass


def retention_audit(worktrees, inventory, proc_root='/proc', now=None, max_age=60):
    """Only direct worktree/target candidates. Never cleans shared caches or sources."""
    worktrees = real_directory(worktrees)
    started = time.monotonic()
    now = time.time() if now is None else now
    if (inventory.get('version') != 1 or inventory.get('complete') is not True
            or inventory.get('process_scope') != 'host'):
        raise Refusal('need complete host-scope control-plane inventory')
    captured = inventory.get('captured_at_unix')
    if type(captured) not in (int, float) or not 0 <= now - captured <= max_age:
        raise Refusal('control-plane inventory is stale or from the future')
    rows = inventory.get('workspaces')
    if not isinstance(rows, list):
        raise Refusal('inventory workspaces must be a list')
    indexed = {}
    identities = {}
    target_identities = {}
    for row in rows:
        raw = row['path']
        # Inventory paths must be absolute strings. Lexical canonicalization
        # alone cannot catch symlink/bind-mount aliases, so resolve symlink
        # spellings and reconcile (st_dev, st_ino) identities for existing
        # workspaces before indexing; any conflicting live row must refuse
        # the whole audit, not be ignored.
        if not isinstance(raw, str) or not os.path.isabs(raw):
            raise Refusal('ambiguous workspace attribution')
        canonical = os.path.normpath(raw)
        if raw != canonical or canonical in indexed:
            raise Refusal('ambiguous workspace attribution')
        if type(row.get('live_run')) is not bool or type(row.get('referenced')) is not bool:
            raise Refusal('ambiguous workspace attribution')
        if not row.get('issue_id') or row.get('status') not in TERMINAL | {
                'backlog', 'todo', 'in_progress', 'in_review', 'blocked'}:
            raise Refusal('missing issue attribution/status')
        try:
            resolved = os.path.realpath(canonical)
        except OSError:
            raise Refusal('ambiguous workspace attribution')
        if resolved != canonical:
            raise Refusal('ambiguous workspace attribution')
        try:
            info = os.stat(canonical)
        except FileNotFoundError:
            identity = None  # path does not exist; no identity to reconcile
        except (PermissionError, OSError):
            raise Refusal(f'incomplete workspace visibility: {canonical}')
        else:
            identity = (info.st_dev, info.st_ino)
            if identity in identities:
                raise Refusal('ambiguous workspace attribution')
            identities[identity] = canonical
        # Workspace roots are not the only alias surface: distinct
        # canonical roots can share one bind-aliased target directory. A
        # terminal/unreferenced row under A and a live/queued/referenced
        # row under B would both pass workspace-identity validation, and
        # without target reconciliation the shared build output is offered
        # as a retention candidate. Reconcile existing target identities
        # across ALL rows — including live/queued/referenced rows outside
        # the candidate directory — and refuse duplicate target attribution.
        try:
            target_info = os.stat(canonical + '/target')
        except FileNotFoundError:
            target_identity = None  # no target yet; nothing to reconcile
        except (PermissionError, OSError):
            raise Refusal(f'incomplete workspace visibility: {canonical}/target')
        else:
            target_identity = (target_info.st_dev, target_info.st_ino)
            if target_identity in target_identities:
                raise Refusal('ambiguous target attribution')
            target_identities[target_identity] = canonical
        indexed[canonical] = row
    refs, deleted = process_references(proc_root)
    # Deleted (unlinked/replaced) artifact references keep their deletion
    # marker: under a container spelling they match neither a host path nor
    # a current workspace inode, so they can never silently count as no
    # reference. Resolve the real on-disk workspace set once so deleted
    # attribution can be checked against every candidate; a deleted entry
    # attributable to nothing still refuses the whole audit, not one row.
    real_workspaces = {}
    for workspace in sorted(worktrees.iterdir()):
        target = workspace / 'target'
        if not target.exists() and not target.is_symlink():
            continue
        if workspace.is_symlink() or target.is_symlink():
            continue
        try:
            real = real_directory(workspace)
        except Refusal:
            continue
        real_workspaces[str(real)] = real
    # Scan workspace inodes once for the final unresolved-deleted check;
    # per-candidate checks below reuse their own traversal.
    all_nodes = set()
    for real in real_workspaces.values():
        all_nodes |= workspace_inodes(real)
    results = []
    for workspace in sorted(worktrees.iterdir()):
        target = workspace / 'target'
        if not target.exists() and not target.is_symlink():
            continue
        result = {'target': str(target), 'eligible': False}
        results.append(result)
        if workspace.is_symlink() or target.is_symlink():
            result['reason'] = 'symlink'
            continue
        workspace = real_directory(workspace)
        target = real_directory(target)
        row = indexed.get(str(workspace))
        if not row:
            result['reason'] = 'unattributed'
            continue
        if row['status'] not in TERMINAL or row['live_run'] or row['referenced']:
            result['reason'] = 'live run, workspace reference, or nonterminal issue'
            continue
        # Filename heuristics cannot establish provenance: an ignored target
        # can hold evidence/archives/sources under Cargo's own subtrees
        # (e.g. debug/incident-20260930.json, debug/snapshot.tar.gz,
        # debug/src/main.py all pass name checks). Eligibility requires an
        # independently recorded, exact-target Operator classification in the
        # inventory row; missing/unknown/mixed provenance stays ineligible.
        # This classification is attested control-plane data, never minted
        # from the filename heuristics below (which remain as a backstop).
        if row.get('target_provenance') != 'build_output_only':
            result['reason'] = ('unclassified or mixed target provenance; '
                                'Operator build-output-only classification required')
            continue
        # -C changes directory but does not neutralize an inherited
        # GIT_INDEX_FILE, GIT_DIR, GIT_WORK_TREE, or GIT_COMMON_DIR: an
        # alternate empty index would hide a force-tracked target file while
        # the ignore rule still passes. Inspect the real repository index
        # regardless of overrides, and refuse when the intended workspace
        # repository cannot be verified.
        git_env = {key: value for key, value in os.environ.items()
                   if key not in {'GIT_INDEX_FILE', 'GIT_DIR', 'GIT_WORK_TREE',
                                  'GIT_COMMON_DIR', 'GIT_NAMESPACE'}}
        try:
            toplevel = subprocess.run(['git', '-C', str(workspace), 'rev-parse',
                                       '--show-toplevel'],
                                      capture_output=True, check=True, env=git_env,
                                      text=True)
        except subprocess.CalledProcessError:
            result['reason'] = 'unverified workspace repository'
            continue
        if Path(toplevel.stdout.strip()) != workspace:
            result['reason'] = 'unverified workspace repository'
            continue
        tracked = subprocess.run(['git', '-C', str(workspace), 'ls-files', '-z', '--', 'target'],
                                 capture_output=True, check=True, env=git_env)
        ignored = subprocess.run(['git', '-C', str(workspace), 'check-ignore', '-q', 'target'],
                                 env=git_env)
        if tracked.stdout or ignored.returncode != 0:
            result['reason'] = 'tracked or not ignored'
            continue
        # .gitignore proves nothing about provenance: an ignored target can
        # still hold preserved evidence/backups/sources. Only Cargo's own
        # top-level output entries pass; anything else vetoes eligibility.
        veto = preservation_veto(target)
        if veto is not None:
            result['reason'] = veto
            continue
        nodes = workspace_inodes(workspace)
        if any(within(path, workspace) or (device, inode) in nodes for path, device, inode in refs):
            result['reason'] = 'actual process cwd/exe/fd/map reference'
            continue
        # Deleted-reference attribution carries its deletion marker from
        # the scan above. An unlinked/replaced artifact under a container
        # spelling cannot be mapped to a host path by lexical match, and
        # its old inode may be gone from the current workspace traversal.
        # A deleted entry whose path reads inside this workspace or whose
        # inode survives in its traversal vetoes this candidate. Removing
        # a replacement never reclaims the already-unlinked mapped inode,
        # so this is a false-negative reference finding, not a deletion
        # plan; no destructive deletion is demonstrated or authorized.
        if any(within(path, workspace) or (device, inode) in nodes
               for path, device, inode in deleted):
            result['reason'] = 'actual process cwd/exe/fd/map reference (deleted artifact)'
            continue
        result.update(eligible=True, allocated_bytes=usage(target), reason='audit only; not deletion authority')
    # Any deleted entry attributable to no audited workspace refuses the
    # whole audit: unresolved namespace mapping must not silently establish
    # no reference. Callers must supply an independently verified
    # exact-path process-reference receipt before any such audit clears.
    if any(not any(within(path, Path(root)) for root in real_workspaces)
           and (device, inode) not in all_nodes
           for path, device, inode in deleted):
        raise Refusal('unresolved deleted process reference; '
                      'exact-path process-reference receipt required')
    if now - captured + time.monotonic() - started > max_age:
        raise Refusal('audit took too long; capture a new inventory')
    return {'version': 1, 'audit_only': True, 'captured_at_unix': now, 'candidates': results}


def filesystem_finding(path, backing_path, floor):
    path = real_directory(path)
    backing_path = real_directory(backing_path)
    if path.stat().st_dev != backing_path.stat().st_dev:
        return {'finding': 'wrong_filesystem', 'monitored_path': str(path),
                'backing_path': str(backing_path)}
    free = available(path)
    if free < floor:
        return {'finding': 'low_available_bytes', 'path': str(path),
                'available_bytes': free, 'floor_bytes': floor}
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    run = commands.add_parser('run')
    run.add_argument('--pool', type=Path, default=DEFAULT_POOL)
    run.add_argument('cargo_args', nargs=argparse.REMAINDER)
    audit = commands.add_parser('audit')
    audit.add_argument('--worktrees', type=Path, required=True)
    audit.add_argument('--inventory', type=Path, required=True)
    retain = commands.add_parser('retain',
                                 help='shared-pool lock-through-mutation retention')
    retain.add_argument('--pool', type=Path, default=DEFAULT_POOL)
    retain.add_argument('--inventory', type=Path, required=True)
    retain.add_argument('--proc-root', type=Path, default=Path('/proc'))
    retain.add_argument('--evidence', type=Path, default=None,
                        help='optional receipt path for the host evidence file')
    watch = commands.add_parser('filesystem')
    watch.add_argument('--path', type=Path, default=Path('/home'))
    watch.add_argument('--backing-path', type=Path, required=True)
    watch.add_argument('--min-available-bytes', type=int, default=10 * GIB)
    args = parser.parse_args()
    try:
        if args.command == 'run':
            cargo_args = args.cargo_args
            if cargo_args[:1] == ['--']:
                cargo_args = cargo_args[1:]
            return run_cargo(args.pool, cargo_args)
        if args.command == 'audit':
            result = retention_audit(args.worktrees, json.loads(args.inventory.read_text()))
        elif args.command == 'retain':
            result = shared_pool_retain(args.pool, json.loads(args.inventory.read_text()),
                                        proc_root=args.proc_root)
            if args.evidence is not None:
                args.evidence.write_text(json.dumps(result, sort_keys=True))
        else:
            if args.min_available_bytes <= 0:
                raise Refusal('available-byte floor must be positive')
            result = filesystem_finding(args.path, args.backing_path, args.min_available_bytes)
            if result is None:
                return 0  # Healthy monitoring is silent.
        print(json.dumps(result, sort_keys=True))
        return 1 if args.command == 'filesystem' else 0
    except (Refusal, OSError, ValueError, KeyError, IndexError, subprocess.CalledProcessError) as error:
        print(json.dumps({'finding': 'refused', 'reason': str(error)}), file=sys.stderr)
        return 75


if __name__ == '__main__':
    sys.exit(main())
