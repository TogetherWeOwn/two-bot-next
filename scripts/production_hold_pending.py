#!/usr/bin/env python3
"""Find when a runtime-affecting main commit first became pending production.

Answers one question from a full-depth local checkout: of the commits on main
after the promoted commit, when did the first one land that deploy-staging
would have built? Metadata only (file names and committer time), never
patches, and no network, fetch or downloaded code.

History must be a first-parent, squash-merge line: promoted is an ancestor of
main, every commit in between has exactly one parent, and the chain from main
reaches promoted. Anything unreadable, shallow, diverged, merge-shaped or over
a cap raises incident_issue.IncidentError; it never reports a false None.

Each commit is judged on its own paths, so a staging run that failed on an
early runtime commit still counts even when a later staging run succeeded.
The ignore list mirrors `on.push.paths-ignore` in deploy-staging.yml; a push
is skipped only when EVERY changed path matches. Renames are split
(--no-renames) so the old path of a runtime file moved into docs still counts.

Bounded-subprocess policy: docs/bounded-subprocess-checks.md.
"""

import os
import re
import subprocess
import threading
from datetime import datetime, timezone
from pathlib import Path

import incident_issue

ROOT = Path(__file__).resolve().parents[1]

GIT_TIMEOUT_SECONDS = 30
MAX_COMMITS = 500
MAX_OUTPUT_BYTES = 1024 * 1024

SHA40 = re.compile(r"[0-9a-f]{40}\Z")

# Mirrors deploy-staging.yml `paths-ignore`; scripts/ci/test_workflows.py pins
# the workflow list and the tests here pin this copy against it.
IGNORED_PREFIXES = (
    "docs/",
    ".github/ISSUE_TEMPLATE/",
    "ops/",
)
IGNORED_EXACT = frozenset({
    "LICENSE",
    ".editorconfig",
    ".gitignore",
    ".gitleaks.toml",
    ".gitleaksignore",
    ".github/pull_request_template.md",
    ".github/CODEOWNERS",
    ".github/dependabot.yml",
})


def _fail(message):
    raise incident_issue.IncidentError(message)


def is_ignored(path):
    """True when deploy-staging's paths-ignore matches this repo-relative path."""
    if path in IGNORED_EXACT or path.startswith(IGNORED_PREFIXES):
        return True
    # '*.md' is root-only: `*` never crosses '/', so nested READMEs deploy.
    return "/" not in path and path.endswith(".md")


def _env():
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env.update({
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_NO_LAZY_FETCH": "1",
        "GIT_NO_REPLACE_OBJECTS": "1",
        "GIT_OPTIONAL_LOCKS": "0",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": os.devnull,
        "LC_ALL": "C",
    })
    return env


def _git(root, args, *, ok=(0,)):
    """Run read-only git (shell=False); return (status, stdout bytes).

    Output is read up to MAX_OUTPUT_BYTES and the child is killed past that or
    past GIT_TIMEOUT_SECONDS. Child stderr is discarded, never echoed.
    """
    try:
        proc = subprocess.Popen(
            ["git", "-C", str(root), *args],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, shell=False, env=_env(),
        )
    except (OSError, ValueError):
        _fail("production hold history git is unavailable")
    timed_out = threading.Event()

    def expire():
        timed_out.set()
        try:
            proc.kill()
        except ProcessLookupError:
            pass

    timer = threading.Timer(GIT_TIMEOUT_SECONDS, expire)
    timer.start()
    try:
        data = proc.stdout.read(MAX_OUTPUT_BYTES + 1)
        too_big = len(data) > MAX_OUTPUT_BYTES
        if too_big:
            proc.kill()
        status = proc.wait()
    finally:
        timer.cancel()
        proc.stdout.close()
    if timed_out.is_set():
        _fail("production hold history git timed out")
    if too_big:
        _fail("production hold history output exceeds the cap")
    if status not in ok:
        if status < 0 or status in (126, 127):
            _fail("production hold history git is unavailable")
        return status, b""
    return status, data


def _require_sha(value):
    if not isinstance(value, str) or not SHA40.match(value):
        _fail("production hold history commit is not a full sha")
    return value


def _first_parent_chain(root, promoted, main):
    status, data = _git(root, [
        "rev-list", "--first-parent", "--parents", "--reverse",
        f"--max-count={MAX_COMMITS + 1}", f"{promoted}..{main}",
    ])
    if status != 0:
        _fail("production hold history is unreadable")
    try:
        lines = data.decode("ascii").splitlines()
    except UnicodeDecodeError:
        _fail("production hold history is malformed")
    if len(lines) > MAX_COMMITS:
        _fail("production hold history exceeds the commit cap")
    chain, previous = [], promoted
    for line in lines:
        fields = line.split(" ")
        if not all(SHA40.match(f) for f in fields):
            _fail("production hold history is malformed")
        if len(fields) != 2:
            _fail("production hold history is not squash first-parent")
        if fields[1] != previous:
            _fail("production hold history is not first-parent from promoted")
        chain.append(fields[0])
        previous = fields[0]
    if previous != main:
        _fail("production hold history is not first-parent from promoted")
    return chain


def _changed_paths(root, parent, commit):
    status, data = _git(root, [
        "diff-tree", "-r", "--name-only", "--no-renames", "-z",
        "--no-ext-diff", parent, commit,
    ])
    if status != 0:
        _fail("production hold history is unreadable")
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        _fail("production hold history is malformed")
    if text and not text.endswith("\0"):
        _fail("production hold history is malformed")
    paths = text.split("\0")[:-1] if text else []
    if any(not p for p in paths):
        _fail("production hold history is malformed")
    return paths


def _commit_time(root, commit):
    status, data = _git(root, ["show", "-s", "--format=%ct", commit])
    text = data.decode("ascii", "replace").strip()
    if status != 0 or not re.fullmatch(r"[0-9]{1,12}", text):
        _fail("production hold history is malformed")
    return datetime.fromtimestamp(int(text), tz=timezone.utc)


def pending_since(promoted, main, root=ROOT):
    """Committer time of the first runtime-affecting commit after promoted.

    Returns an aware UTC datetime, or None when every commit after promoted
    (or none) would be skipped by deploy-staging. A commit with no changed
    paths is treated as runtime-affecting (conservative).
    """
    promoted = _require_sha(promoted)
    main = _require_sha(main)
    status, data = _git(root, ["rev-parse", "--is-shallow-repository"])
    if status != 0 or data.strip() != b"false":
        _fail("production hold history checkout is shallow or unreadable")
    for sha in (promoted, main):
        if _git(root, ["cat-file", "-e", f"{sha}^{{commit}}"])[0] != 0:
            _fail("production hold history commit is missing")
    status, _ = _git(root, ["merge-base", "--is-ancestor", promoted, main],
                     ok=(0, 1))
    if status != 0:
        _fail("promoted commit is not an ancestor of main")
    if promoted == main:
        return None
    previous = promoted
    for commit in _first_parent_chain(root, promoted, main):
        paths = _changed_paths(root, previous, commit)
        if not paths or not all(is_ignored(p) for p in paths):
            return _commit_time(root, commit)
        previous = commit
    return None
