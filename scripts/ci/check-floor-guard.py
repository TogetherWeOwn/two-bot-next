#!/usr/bin/env python3
"""HARD gate: the CONSTRAINTS.md floor holds on every diff.

Fails when a diff adds a checker waiver, adds a test skip, deletes a test
file, strips assertions from test code, or lowers a number in CONSTRAINTS.md.
Tightening the bar is silent; loosening it is loud.

Contract (reference: constraint-driven-development floor-guard):
- Input is the merge-base-to-selected-head diff plus untracked files.
- Exit 0 clean, 1 on a floor violation, 2 when the guard cannot run
  (no merge base). A 2 never reads as a 0.
- Markdown prose (.md) is exempt from the waiver/skip patterns: mentioning a
  lint attribute in docs is not adding one.
- Exceptions come from the merge base's CONSTRAINTS.md, never the head's, so
  a PR cannot exempt its own waiver. A row names a path glob, the waiver text,
  an owner and an expiry date, and stops covering after that date.
- A database ignore that names the agent test DB (or a disposable test
  database) is accepted in an integration target or a crate src tree, where
  the onboarding and gateway test lanes run the ignored DB scenarios.
  check-test-inventory.py requires a CI lane to run each such target.
- Assertions are counted across the whole diff: a moved assertion passes, a
  dropped one without a replacement fails.

Rust attributes only take effect at line start, so the gate honours them
there; prose mentions and fixture strings elsewhere never match. Inline
waivers (trailing comments) are matched anywhere.

The inline patterns below are string-concatenated so this file's own
committed diff carries none of the contiguous literals it scans for.
"""

import argparse
import fnmatch
import re
import shlex
import subprocess
import sys
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONSTRAINTS = "CONSTRAINTS.md"


class NoMergeBase(Exception):
    """Raised when no merge base exists against the base ref."""


# A waiver attribute only takes effect at the start of a line, so only that
# position is flagged. Doc prose and fixture strings never sit there.
# `expect` silences the same lints as `allow`, and a `cfg_attr` wrapper
# silences them conditionally, so all three forms are waivers here. A
# conditional ignore is reported as a waiver (one violation, not two).
ATTR_ALLOW = re.compile(r"^\s*#\s*!?\s*\[\s*(?:allow|expect)\b")
ATTR_CFG_WAIVER = re.compile(
    r"^\s*#\s*!?\s*\[.*cfg_attr\s*\(.*\b(?:allow|expect|ignore)\b"
)
ATTR_RUSTFMT_SKIP = re.compile(r"^\s*#\s*\[\s*rustfmt::skip")
ATTR_IGNORE = re.compile(r"^\s*#\s*\[\s*ignore")
ROUTED_DB_IGNORE = re.compile(
    r'^\s*#\s*\[\s*ignore\s*=\s*"[^"]*(?:agent-testdb|TEST_DATABASE_URL|disposable[^"]*database)[^"]*"\s*\]',
    re.IGNORECASE,
)
INTEGRATION_TARGET = re.compile(r"^crates/[^/]+/tests/.+\.rs$")
SRC_ROUTED_TARGET = re.compile(r"^crates/[^/]+/src/.*\.rs$")

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

# Focused-test markers are language-specific: Rust's ordinary iterator
# .skip() is not a test directive, while the same suffix in JS test runners is.
JS_TEST_SKIP = re.compile(
    r"\.(?:skip|todo)\s*\(|(?<!\w)x(?:it|describe)\s*\("
)
PYTHON_TEST_SKIP = re.compile(r"pytest\.mark\.skip")
GO_TEST_SKIP = re.compile(r"t\.Skip\s*\(")

TEST_PATH = re.compile(r"\.test\.|_tests?\.|/tests?/|_tests/|/tests?\.rs$|test_")
ASSERTS = re.compile(r"\bassert(_\w+)?!?|\bexpect\s*\(|\bshould\b")
SECTION = re.compile(r"^##\s+(.+?)\s*$")


def _is_markdown(path):
    return path.endswith(".md")


def _numbers(line):
    without_dates = re.sub(r"\b\d{4}-\d{2}-\d{2}\b", "", line)
    return [float(n) for n in re.findall(r"\d+(?:\.\d+)?", without_dates)]


def _words(line):
    """Lowercase words with dates and numbers stripped out.

    Used to tell a reworded Enforced row apart from a tightened number:
    ``Zero clippy warnings`` -> ``Clippy warnings are advisory`` drops the
    word ``zero``, while ``Per RFC 7, ... >= 80%`` only adds words.
    """
    text = re.sub(r"\b\d{4}-\d{2}-\d{2}\b", " ", line)
    text = re.sub(r"\d+(?:\.\d+)?%?", " ", text)
    return set(re.findall(r"[a-z]+", text.lower()))


def _cells(line):
    return [cell.strip() for cell in line.strip().strip("|").split("|")]


def _table_rows(text):
    """Return (section, key, line) for each table row; key is the first cell."""
    rows = []
    section = ""
    for line in (text or "").splitlines():
        heading = SECTION.match(line)
        if heading:
            section = heading.group(1).lower()
            continue
        cells = line.strip().split("|")
        if len(cells) < 3:
            continue
        key = cells[1].strip()
        if not key or set(key) <= {"-", ":"} or key.lower() == "dimension":
            continue
        rows.append((section, key, line))
    return rows


def _op_numbers(line):
    """Numbers with a comparison operator: the operative thresholds.

    A parenthetical like ``(was 80%)`` or a bare count like ``100 files``
    or ``RFC 7`` carries no operator, so it is history or context, not the
    bar itself.
    """
    return [float(n) for n in re.findall(r"(?:>=|<=|>|<|=)\s*(\d+(?:\.\d+)?)", line)]


def _pct_numbers(line):
    return [float(n) for n in re.findall(r"(\d+(?:\.\d+)?)\s*%", line)]


def _lowered(old, new):
    # Thresholds are percents (``>= 80%``, ``at least 50%``); a bare
    # operator count like ``>= 100 files`` is context, not the bar. Compare
    # percents when the old row has them so a lowered bar cannot hide behind
    # a large reference number; fall back to operator numbers, then to all
    # numbers, for rows without one. A parenthetical like ``(was 80%)``
    # keeps its number, so lowering plus a history note still reads as
    # lowered, while raising with a note passes. Sorted ascending so a
    # reordered pair compares like-for-like.
    old_pct = sorted(_pct_numbers(old))
    if old_pct:
        new_pct = sorted(_pct_numbers(new))
        if len(new_pct) < len(old_pct):
            return True
        return any(n < o for o, n in zip(old_pct, new_pct))
    old_ops = sorted(_op_numbers(old))
    if old_ops:
        new_ops = sorted(_op_numbers(new))
        if len(new_ops) < len(old_ops):
            return True
        return any(n < o for o, n in zip(old_ops, new_ops))
    old_nums = sorted(_numbers(old))
    new_nums = sorted(_numbers(new))
    if len(new_nums) < len(old_nums):
        return True
    return any(n < o for o, n in zip(old_nums, new_nums))


def _promoted(old, head_rows, base_keys):
    """A measured row may move to Enforced under a new name if it is no looser.

    The bar lives in the Measured Today cell and the Enforced Rule cell
    (both the second column), so only those cells are compared: a command
    or flag in the new Checked-by cell must not read as a lowered bar.
    """
    if not _numbers(old):
        return False
    return any(
        section.startswith("enforced")
        and key not in base_keys
        and not _lowered(_rule_cell(old), _rule_cell(line))
        for section, key, line in head_rows
    )


def _rule_cell(line):
    """The Rule cell (second column) of a CONSTRAINTS.md table row."""
    cells = _cells(line)
    return cells[1] if len(cells) >= 2 else line


def _warn_first_added(old_rule, head_rule):
    """Whether the head Rule cell gains a WARN-first demotion marker."""
    return bool((_words(head_rule) - _words(old_rule)) & {"warn", "first"})


def _scope_cells(line):
    """The (Checked-by, Runs-at) cells of an Enforced table row."""
    cells = _cells(line)
    if len(cells) < 4:
        return "", ""
    return cells[2], cells[3]


def _checked_by_narrowed(base_cell, head_cell):
    """Whether a Checked-by edit removes a command or one of its flags."""
    commands = re.findall(r"`([^`]+)`", base_cell)
    head_commands = re.findall(r"`([^`]+)`", head_cell)
    for command in commands:
        try:
            tokens = shlex.split(command)
        except ValueError:
            return True
        if not tokens:
            continue
        script = next((token for token in tokens[1:] if "/" in token), None)
        anchor = script or tokens[0]
        if not any(anchor in candidate for candidate in head_commands):
            return True
        for index, token in enumerate(tokens):
            if not token.startswith("-"):
                continue
            required = token
            if index + 1 < len(tokens) and not tokens[index + 1].startswith("-"):
                required += " " + tokens[index + 1]
            if not any(required in candidate for candidate in head_commands):
                return True
    return False


def _schedule_rank(cell):
    """Rank the repository's supported CI schedules by breadth."""
    text = " ".join(_words(cell))
    if "every" in text and "rust" in text and "pr" in text:
        return 3
    if "every" in text and "pr" in text:
        return 4
    if "nightly" in text:
        return 2
    if "manual" in text:
        return 1
    return None


def _runs_at_narrowed(base_cell, head_cell):
    base_rank = _schedule_rank(base_cell)
    head_rank = _schedule_rank(head_cell)
    if base_rank is not None:
        return head_rank is None or head_rank < base_rank
    if head_rank is not None:
        return False
    base_words = _words(base_cell)
    return bool(base_words) and not (base_words & _words(head_cell))


def threshold_violations(base_text, head_text):
    """Compare CONSTRAINTS.md rows across the diff; a deleted file is scan()'s job.

    An Enforced key must stay in an enforced section: moving it under
    Measured is a demotion and fails, and removing it fails even when an
    unrelated Enforced row is added (a rename must keep its key). An
    Enforced row that keeps its key but drops words from its Rule cell is a
    rewording and fails, while added words alone pass so tightening stays
    silent. Numbers are compared on the Rule cell only, so a command or flag
    edit elsewhere on the row never reads as a lowered threshold. Dropping
    the ``(WARN first)`` marker graduates a gate to HARD and passes, but
    adding the marker back demotes the gate and fails; replacing the
    Checked-by or Runs-at cell wholesale fails as a narrowed scope, while
    rewording either cell around its substance passes.
    """
    if base_text is None or head_text is None:
        return []
    base_rows = _table_rows(base_text)
    head_rows = _table_rows(head_text)
    base_keys = {key for _, key, _ in base_rows}
    errors = []
    for section, key, old in base_rows:
        if section == "exceptions":
            continue
        if section.startswith("enforced"):
            enforced_match = [
                line
                for head_section, k, line in head_rows
                if k == key and head_section.startswith("enforced")
            ]
            if enforced_match:
                head_line = enforced_match[0]
                if _lowered(_rule_cell(old), _rule_cell(head_line)):
                    errors.append(
                        "threshold-lowered: CONSTRAINTS.md weakened: "
                        f"{old.strip()[:80]} -> {head_line.strip()[:80]}"
                    )
                elif _warn_first_added(_rule_cell(old), _rule_cell(head_line)):
                    errors.append(
                        "threshold-lowered: CONSTRAINTS.md Enforced row demoted "
                        f"to WARN-first: {old.strip()[:80]} -> {head_line.strip()[:80]}"
                    )
                elif _words(_rule_cell(old)) - _words(_rule_cell(head_line)) - {
                    "warn",
                    "first",
                }:
                    errors.append(
                        "threshold-lowered: CONSTRAINTS.md Enforced row reworded: "
                        f"{old.strip()[:80]} -> {head_line.strip()[:80]}"
                    )
                else:
                    old_checked, old_runs = _scope_cells(old)
                    head_checked, head_runs = _scope_cells(head_line)
                    if _runs_at_narrowed(old_runs, head_runs):
                        errors.append(
                            "threshold-lowered: CONSTRAINTS.md Enforced row "
                            "Runs-at scope narrowed: "
                            f"{old.strip()[:80]} -> {head_line.strip()[:80]}"
                        )
                    elif _checked_by_narrowed(old_checked, head_checked):
                        errors.append(
                            "threshold-lowered: CONSTRAINTS.md Enforced row "
                            "Checked-by scope narrowed: "
                            f"{old.strip()[:80]} -> {head_line.strip()[:80]}"
                        )
            elif any(k == key for _, k, _ in head_rows):
                errors.append(
                    "threshold-lowered: CONSTRAINTS.md Enforced row demoted "
                    f"out of Enforced: {old.strip()[:80]}"
                )
            else:
                errors.append(
                    f"threshold-lowered: CONSTRAINTS.md row removed: {old.strip()[:80]}"
                )
            continue
        same_key = [line for _, k, line in head_rows if k == key]
        if same_key:
            if _lowered(_rule_cell(old), _rule_cell(same_key[0])):
                errors.append(
                    "threshold-lowered: CONSTRAINTS.md weakened: "
                    f"{old.strip()[:80]} -> {same_key[0].strip()[:80]}"
                )
        elif not (section.startswith("measured") and _promoted(old, head_rows, base_keys)):
            errors.append(f"threshold-lowered: CONSTRAINTS.md row removed: {old.strip()[:80]}")
    return errors


def _exceptions(text):
    """Waiver rows from the Exceptions table as (path glob, waiver text, expiry)."""
    rows = []
    for section, _, line in _table_rows(text):
        if section != "exceptions":
            continue
        cells = _cells(line)
        if len(cells) < 4 or not all(cells[:3]):
            continue
        try:
            expires = date.fromisoformat(cells[3])
        except ValueError:
            continue
        rows.append((cells[0], cells[1].strip("`"), expires))
    return rows


def _exempt(path, line, exceptions, today):
    return any(
        fnmatch.fnmatchcase(path, glob) and waiver in line and today <= expires
        for glob, waiver, expires in exceptions
    )


def _is_routed_db_ignore(path, line):
    if not ROUTED_DB_IGNORE.match(line):
        return False
    return bool(INTEGRATION_TARGET.match(path) or SRC_ROUTED_TARGET.match(path))


def _is_test_skip(path, line):
    if path.endswith(".rs"):
        return bool(ATTR_IGNORE.match(line)) and not _is_routed_db_ignore(path, line)
    if not TEST_PATH.search(path):
        return False
    if path.endswith((".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts")):
        return bool(JS_TEST_SKIP.search(line))
    if path.endswith(".py"):
        return bool(PYTHON_TEST_SKIP.search(line))
    if path.endswith(".go"):
        return bool(GO_TEST_SKIP.search(line))
    return False


def _is_silenced(line):
    return bool(
        ATTR_ALLOW.match(line)
        or ATTR_CFG_WAIVER.match(line)
        or ATTR_RUSTFMT_SKIP.match(line)
        or INLINE_SUPPRESSIONS.search(line)
    )


def _is_assertion(path, line):
    return bool(not _is_markdown(path) and TEST_PATH.search(path) and ASSERTS.search(line))


def scan(added, removed, deleted, exceptions=(), today=None):
    """Check synthetic diff sets. Each of added/removed holds (path, line).

    Returns a list of human-readable violation strings (empty when clean).
    """
    today = today or date.today()
    errors = []

    for path, line in added:
        if _is_markdown(path):
            continue
        if _is_silenced(line) and not _exempt(path, line, exceptions, today):
            errors.append(f"silenced-checker: waiver added in {path}: {line.strip()[:120]}")
        if _is_test_skip(path, line) and not _exempt(path, line, exceptions, today):
            errors.append(f"test-made-easier: skip added in {path}: {line.strip()[:120]}")

    for path in deleted:
        if TEST_PATH.search(path):
            errors.append(f"deleted-test: test file removed: {path}")
        if Path(path).name == CONSTRAINTS:
            errors.append("threshold-lowered: CONSTRAINTS.md deleted")

    dropped = [path for path, line in removed if _is_assertion(path, line)]
    gained = [path for path, line in added if _is_assertion(path, line)]
    net = len(dropped) - len(gained)
    if net > 0:
        paths = ", ".join(sorted(set(dropped)))
        errors.append(
            f"assertion-removed: {net} assertion(s) dropped from test code: {paths[:200]}"
        )

    return errors


def _git(root, *args):
    return subprocess.run(
        ["git", *args], cwd=root, capture_output=True, text=True
    )


def _merge_base(root, base_ref, head_ref):
    mb = _git(root, "merge-base", base_ref, head_ref)
    if mb.returncode != 0:
        raise NoMergeBase(f"no merge base against {base_ref}")
    return mb.stdout.strip()


def _show(root, ref, path):
    shown = _git(root, "show", f"{ref}:{path}")
    return shown.stdout if shown.returncode == 0 else None


def collect(root, base, head_ref="HEAD"):
    """Collect (added, removed, deleted) from the base-to-head diff."""
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


def check(root=ROOT, base_ref="origin/main", head_ref="HEAD", today=None):
    """Run the guard against the selected head. Raises NoMergeBase."""
    root = Path(root)
    base = _merge_base(root, base_ref, head_ref)
    added, removed, deleted = collect(root, base, head_ref)
    base_constraints = _show(root, base, CONSTRAINTS)
    head_constraints = _show(root, head_ref, CONSTRAINTS)
    errors = scan(
        added,
        removed,
        deleted,
        exceptions=_exceptions(base_constraints),
        today=today,
    )
    return errors + threshold_violations(base_constraints, head_constraints)


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
            "Fix the code, or record a reasoned exception in CONSTRAINTS.md on main.",
            file=sys.stderr,
        )
        raise SystemExit(1)
    print("floor-guard: clean")
    raise SystemExit(0)


if __name__ == "__main__":
    main()
