#!/usr/bin/env python3
"""Compare staging migration sources with an already verified, clean plan.

The caller supplies the plan's source SHA after verifying both installed
ledgers. This check proves migration-source equality only. It does not read
the database, verify runtime bindings or authorize a staging transition.
"""

import argparse
import os
import re
import subprocess
import sys


SHA = re.compile(r"[0-9a-f]{40}")
MIGRATION_PATHS = ("crates/cutover/migrations", "crates/store/migrations")
EMPTY_TREE = b"4b825dc642cb6eb9a060e54bf8d69288fbee4904"


class Refused(ValueError):
    """A refusal whose message is safe to emit in workflow logs."""


def git_environment():
    # Repository and object routing come from the explicit checkout path.
    return {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}


def git(repo, *args):
    try:
        return subprocess.run(
            ["git", "--no-replace-objects", "-C", repo, *args],
            env={**git_environment(), "GIT_NO_LAZY_FETCH": "1"},
            check=True, capture_output=True, timeout=10,
        ).stdout
    except (OSError, subprocess.SubprocessError):
        raise Refused("migration_source_unavailable") from None


def migration_trees(repo, sha):
    if not isinstance(sha, str) or SHA.fullmatch(sha) is None:
        raise Refused("invalid_source_sha")
    if git(repo, "cat-file", "-t", sha) != b"commit\n":
        raise Refused("invalid_source_commit")
    entries = git(repo, "ls-tree", "--full-tree", "-z", sha, "--", *MIGRATION_PATHS).split(b"\0")
    if len(entries) != len(MIGRATION_PATHS) + 1 or entries[-1] != b"":
        raise Refused("migration_tree_unavailable")
    trees = []
    for path, entry in zip(MIGRATION_PATHS, entries[:-1]):
        match = re.fullmatch(rb"040000 tree ([0-9a-f]{40})\t" + re.escape(path.encode()), entry)
        if match is None or match[1] == EMPTY_TREE:
            raise Refused("migration_tree_unavailable")
        trees.append(match[1])
    return tuple(trees)


def verify(repo, plan_source_sha, forward_sha, recovery_sha):
    expected = migration_trees(repo, plan_source_sha)
    if migration_trees(repo, forward_sha) != expected:
        raise Refused("forward_migration_source_mismatch")
    if migration_trees(repo, recovery_sha) != expected:
        raise Refused("recovery_migration_source_mismatch")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--plan-source-sha", required=True)
    parser.add_argument("--forward-sha", required=True)
    parser.add_argument("--recovery-sha", required=True)
    args = parser.parse_args(argv)
    try:
        verify(args.repo, args.plan_source_sha, args.forward_sha, args.recovery_sha)
    except Refused as error:
        print(error, file=sys.stderr)
        return 1
    print("staging_migration_sources_match")
    return 0


if __name__ == "__main__":
    sys.exit(main())
