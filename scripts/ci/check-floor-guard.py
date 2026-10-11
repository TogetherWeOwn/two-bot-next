#!/usr/bin/env python3
"""HARD gate: the CONSTRAINTS.md floor holds on every diff.

Fails when a diff adds a checker waiver, adds a test skip, deletes a test
file, strips an assertion from a test that stays, or lowers a number in
CONSTRAINTS.md. Tightening the bar is silent; loosening it is loud.

Contract (reference: constraint-driven-development floor-guard):
- Input is the merge-base-to-selected-head diff plus untracked files.
- Exit 0 clean, 1 on a floor violation, 2 when the guard cannot run
  (no merge base). A 2 never reads as a 0.
- Markdown prose (.md) is exempt from the waiver/skip patterns: mentioning a
  lint attribute in docs is not adding one. Rule text lives in CONSTRAINTS.md
  itself, which only gets the threshold check.

Rust attributes only take effect at line start, so the gate honours them
there; prose mentions and fixture strings elsewhere never match. Inline
waivers (trailing comments) are matched anywhere.

The inline patterns below are string-concatenated so this file's own
committed diff carries none of the contiguous literals it scans for.
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


class NoMergeBase(Exception):
    """Raised when no merge base exists against the base ref."""


# A waiver attribute only takes effect at the start of a line, so only that
# position is flagged. Doc prose and fixture strings never sit there.
ATTR_ALLOW = re.compile(r"^\s*#\s*!?\s*\[\s*allow")
ATTR_RUSTFMT_SKIP = re.compile(r"^\s*#\s*\[\s*rustfmt::skip")
ATTR_IGNORE = re.compile(r"^\s*#\s*\[\s*ignore")

# Trailing-comment waivers take effect mid-line, so they match anywhere.
INLINE_SUPPRESSIONS = re.compile(
    "|".join(
        [
            "eslint-" + "disable",
            "biome-" + "ignore",
            "@ts-" + "ignore",
            "@ts-" + "nocheck",
            "# *" + "noqa",
            "# *type:" + " *ignore",
            "istanbul " + "ignore",
            "nosem" + "grep",
            "gitleaks:" + "allow",
            "Stryker " + "disable",
        ]
    )
)

# Focused-test markers that take effect mid-line. The bare-name markers carry
# a lookbehind so words that merely contain them (SystemExit, process::exit)
# never match: only a real focused-test call counts.
SKIP_CALLS = re.compile(
    "|".join(
        [
            r"\." + r"skip\s*\(",
            r"\." + r"todo\s*\(",
            r"(?<!\w)x" + r"it\s*\(",
            r"(?<!\w)x" + r"describe\s*\(",
            r"pytest\.mark\." + "skip",
            r"t\." + r"Skip\s*\(",
        ]
    )
)

TEST_PATH = re.compile(r"\.test\.|_test\.|/tests?/|test_")
ASSERTS = re.compile(r"\bassert(_\w+)?!?|\bexpect\s*\(|\bshould\b")


def _numbers(line):
    return [float(n) for n in re.findall(r"\d+(?:\.\d+)?", line)]


def _is_markdown(path):
    return path.endswith(".md")


def _is_constraints(path):
    return Path(path).name == "CONSTRAINTS.md"


def scan(added, removed, deleted):
    """Check synthetic diff sets. Each of added/removed holds (path, line).

    Returns a list of human-readable violation strings (empty when clean).
    """
    errors = []

    for path, line in added:
        if _is_markdown(path):
            continue
        if ATTR_ALLOW.match(line) or ATTR_RUSTFMT_SKIP.match(line):
            errors.append(f"silenced-checker: waiver added in {path}: {line.strip()[:120]}")
        elif INLINE_SUPPRESSIONS.search(line):
            errors.append(f"silenced-checker: waiver added in {path}: {line.strip()[:120]}")
        if ATTR_IGNORE.match(line) or SKIP_CALLS.search(line):
            errors.append(f"test-made-easier: skip added in {path}: {line.strip()[:120]}")

    for path in deleted:
        if TEST_PATH.search(path):
            errors.append(f"deleted-test: test file removed: {path}")

    for path, line in removed:
        if TEST_PATH.search(path) and ASSERTS.search(line):
            errors.append(
                f"assertion-removed: assertion dropped from {path}: {line.strip()[:120]}"
            )

    removed_limits = [line for path, line in removed if _is_constraints(path)]
    added_limits = [line for path, line in added if _is_constraints(path)]
    for old, new in zip(removed_limits, added_limits):
        old_nums, new_nums = _numbers(old), _numbers(new)
        for i, n in enumerate(new_nums):
            if i < len(old_nums) and n < old_nums[i]:
                errors.append(
                    f"threshold-lowered: CONSTRAINTS.md weakened: "
                    f"{old.strip()[:80]} -> {new.strip()[:80]}"
                )
                break

    return errors


def _git(root, *args):
    return subprocess.run(
        ["git", *args], cwd=root, capture_output=True, text=True
    )


def collect(root, base_ref, head_ref="HEAD"):
    """Collect (added, removed, deleted) from the merge-base-to-head diff."""
    mb = _git(root, "merge-base", base_ref, head_ref)
    if mb.returncode != 0:
        raise NoMergeBase(f"no merge base against {base_ref}")
    base = mb.stdout.strip()

    diff = _git(root, "diff", "--unified=0", "--no-color", base, head_ref, "--")
    added, removed = [], []
    path = ""
    for line in diff.stdout.splitlines():
        if line.startswith("+++ "):
            path = line[6:].removeprefix("b/")
        elif line.startswith("+") and not line.startswith("+++"):
            added.append((path, line[1:]))
        elif line.startswith("-") and not line.startswith("---"):
            removed.append((path, line[1:]))

    status = _git(root, "diff", "--name-status", base, head_ref, "--")
    deleted = [
        line.split("\t", 1)[1]
        for line in status.stdout.splitlines()
        if line.startswith("D\t")
    ]

    others = _git(root, "ls-files", "--others", "--exclude-standard")
    for name in others.stdout.split():
        try:
            text = (root / name).read_text(errors="replace")
        except OSError:
            continue
        for line in text.splitlines():
            added.append((name, line))

    return added, removed, deleted


def check(root=ROOT, base_ref="origin/main", head_ref="HEAD"):
    """Run the guard against the selected head. Raises NoMergeBase."""
    added, removed, deleted = collect(Path(root), base_ref, head_ref)
    return scan(added, removed, deleted)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument("--head-ref", default="HEAD")
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args(argv)
    try:
        errors = check(args.root, args.base_ref, args.head_ref)
    except NoMergeBase as exc:
        print(f"floor-guard: {exc}", file=sys.stderr)
        raise SystemExit(2)
    for error in errors:
        print(f"floor-guard: {error}", file=sys.stderr)
    if errors:
        print(
            "floor-guard: each is a move that lowers the bar. "
            "Fix the code instead of silencing the check.",
            file=sys.stderr,
        )
        raise SystemExit(1)
    print("floor-guard: clean")
    raise SystemExit(0)


if __name__ == "__main__":
    main()
