#!/usr/bin/env python3
"""HARD gate: changed-lines coverage must stay at or above the threshold.

Inputs are a ``cargo llvm-cov --lcov`` report and a zero-context
``git diff base head`` over Rust files. A changed line counts when lcov lists
it as executable (a ``DA`` record); it is covered when its execution count is
positive. The gate reports covered/coverable over the intersection and fails
below ``--threshold`` percent (default 80). A patch with no coverable changed
lines passes with a note. A changed line in a measured Rust source file
(``crates/*/src/**`` or ``src/**``) that has no lcov record at all counts as
uncovered when the file defines a function body, so uninstrumented code can
never pass as "nothing to measure" while functionless module/re-export files
remain outside the executable-line denominator.

HARD mode: a shortfall prints ``::error::`` annotations and exits 1. A blind
gate (missing or unusable lcov, unreadable diff) also errors and exits 1; it
must never turn a blind PR green. The check.yml measurement wrapper uses
``--ignore-run-fail`` to collect coverage despite unit-test failures, which are
owned HARD by the rust-tests lane. Build or report-generation failures still
fail this lane. Malformed arguments exit non-zero.
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MAX_ANNOTATIONS = 10


def read_lcov(path):
    """Map of (file, line) -> hit count for executable lines."""
    try:
        text = Path(path).read_text()
    except OSError as exc:
        return None, f"lcov report unreadable: {exc}"
    if not text.strip():
        return None, "lcov report is empty"
    hits = {}
    current = None
    data_records = 0
    malformed_records = False
    for raw in text.splitlines():
        if raw.startswith("SF:"):
            if current is not None:
                malformed_records = True
            current = raw[3:].strip() or None
        elif raw.startswith("DA:"):
            parts = raw[3:].split(",")
            try:
                line, count = int(parts[0]), int(parts[1])
            except (ValueError, IndexError):
                malformed_records = True
                continue
            if current is None or line < 1 or count < 0:
                malformed_records = True
                continue
            hits[(current, line)] = count
            data_records += 1
        elif raw.strip() == "end_of_record":
            current = None
    if current is not None:
        malformed_records = True
    if malformed_records:
        return None, "lcov report contains malformed DA records"
    if not data_records:
        return None, "lcov report contains no valid DA records"
    return hits, ""


_C_SIMPLE_ESCAPES = {
    "a": 7, "b": 8, "t": 9, "n": 10, "v": 11, "f": 12, "r": 13,
    '"': 34, "\\": 92,
}


def _decode_git_c_quoted(token):
    """Decode a Git C-quoted path token (``"b/..."``) to its path string.

    Git quotes a diff path when it contains bytes outside printable ASCII
    (or a quote/backslash): the whole ``b/<path>`` is wrapped in double
    quotes, special characters use C escapes (``\\t``, ``\\"``, ``\\\\``)
    and every other non-printable byte is a three-digit octal escape, so
    non-ASCII names arrive as octal-escaped UTF-8 bytes. Decode the bytes
    first, then UTF-8, to recover the repository-relative name coverage
    data uses. Raises ValueError on anything Git would not emit.
    """
    if len(token) < 2 or not token.startswith('"') or not token.endswith('"'):
        raise ValueError(f"unterminated quoted diff path: {token!r}")
    inner = token[1:-1]
    out = bytearray()
    pos = 0
    while pos < len(inner):
        ch = inner[pos]
        if ch == '"':
            raise ValueError(f"unescaped quote in quoted diff path: {token!r}")
        if ch != "\\":
            out.extend(ch.encode("utf-8"))
            pos += 1
            continue
        if pos + 1 >= len(inner):
            raise ValueError(f"trailing backslash in quoted diff path: {token!r}")
        esc = inner[pos + 1]
        if esc in "01234567":
            digits = inner[pos + 1: pos + 4]
            if len(digits) != 3 or any(c not in "01234567" for c in digits):
                raise ValueError(f"bad octal escape in quoted diff path: {token!r}")
            out.append(int(digits, 8))
            pos += 4
        elif esc in _C_SIMPLE_ESCAPES:
            out.append(_C_SIMPLE_ESCAPES[esc])
            pos += 2
        else:
            raise ValueError(f"unsupported escape \\{esc} in quoted diff path: {token!r}")
    try:
        return bytes(out).decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ValueError(f"quoted diff path is not valid UTF-8: {token!r}") from exc


def _parse_new_file_header(raw):
    """B-side path from a ``+++ `` diff header, or None for ``/dev/null``.

    Accepts the two shapes Git emits -- ``+++ b/<path>`` and the C-quoted
    ``+++ "b/<path>"`` -- and raises ValueError for anything else, so a
    malformed header fails the gate closed instead of leaving the
    preceding file selected.
    """
    if not raw.startswith("+++ "):
        raise ValueError(f"unsupported +++ diff header: {raw!r}")
    target = raw[len("+++ "):]
    # Git appends a single TAB to ---/+++ header lines when the path
    # contains a space (verified with Git 2.47.3, -U0 --no-renames,
    # core.quotepath=true, for both plain and C-quoted headers). That tab
    # is a header terminator, not part of the file name: a literal tab in
    # a name would arrive C-escaped, so strip exactly one trailing TAB
    # before decoding. Without this, spaced names keep the tab, miss
    # their LCOV record, and fall through to the no-coverable-lines
    # success path (fail-open), while quoted spaced names fail to parse.
    if target.endswith("\t"):
        target = target[:-1]
    if not target:
        raise ValueError("empty +++ diff header")
    if target == "/dev/null":
        return None
    if target.startswith('"'):
        decoded = _decode_git_c_quoted(target)
        if decoded == "/dev/null":
            return None
        if decoded.startswith("b/") and decoded[2:]:
            return decoded[2:]
        raise ValueError(f"quoted +++ header is not a b/ path: {raw!r}")
    if target.startswith("b/") and target[2:]:
        return target[2:]
    raise ValueError(f"unsupported +++ diff header: {raw!r}")


def added_lines(base_ref, head_ref):
    """Added (file, new-line) pairs from a zero-context diff of Rust files."""
    try:
        proc = subprocess.run(
            ["git", "-C", str(ROOT), "diff", "-U0", "--no-color", "--no-renames",
             f"{base_ref}", f"{head_ref}", "--", "*.rs"],
            capture_output=True,
            text=True,
            timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, f"git diff did not run: {exc}"
    if proc.returncode != 0:
        return None, f"git diff exited {proc.returncode}: {proc.stderr.strip()[:200]}"
    added = set()
    path, new_line = None, 0
    in_hunk = False
    header_seen = False
    for raw in proc.stdout.splitlines():
        if raw.startswith("diff --git "):
            # Every file boundary resets the selected file: lines from a
            # file whose header is missing or malformed must never be
            # attributed to the preceding file.
            path, in_hunk, header_seen = None, False, False
            continue
        if not in_hunk and raw.startswith("+++"):
            try:
                path = _parse_new_file_header(raw)
            except ValueError as exc:
                return None, f"unreadable diff header ({exc})"
            header_seen = True
            continue
        match = re.match(r"@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@", raw)
        if match:
            if not header_seen:
                return None, "diff hunk without a +++ file header; failing closed"
            new_line = int(match.group(1))
            in_hunk = True
            continue
        if not in_hunk or path is None:
            continue
        if raw.startswith("+"):
            added.add((path, new_line))
            new_line += 1
        elif raw.startswith("-"):
            continue
        elif raw.startswith("\\"):
            # "\ No newline at end of file" annotates the previous line.
            continue
        else:
            new_line += 1
    return added, ""


def normalize(path):
    """Match lcov source paths (absolute or workspace-relative) to diff paths."""
    text = str(path)
    try:
        return str(Path(text).relative_to(ROOT))
    except ValueError:
        return text.lstrip("./")


def is_measurable_source(path):
    """Workspace-relative Rust sources the lib+bins+tests report must instrument.

    Test-only files (``*_test.rs``, ``*_tests.rs``, ``test_*.rs``,
    ``tests.rs``, or anything under a ``tests`` directory) are excluded:
    changing only tests must not fail the gate.
    """
    if not path.endswith(".rs"):
        return False
    parts = path.split("/")
    filename = parts[-1]
    if filename in ("tests.rs",):
        return False
    if (filename.startswith("test_") or filename.endswith("_test.rs")
            or filename.endswith("_tests.rs")):
        return False
    if "tests" in parts:
        return False
    if len(parts) >= 4 and parts[0] == "crates" and parts[2] == "src":
        return True
    if len(parts) >= 2 and parts[0] == "src":
        return True
    return False


def _contains_fn_body(source):
    """Whether any ``fn`` item has a ``{`` body before its closing ``;``.

    Linear scan (no regular expressions): ``;`` inside ``()``/``[]`` pairs
    (for example ``[u8; 32]``) never terminates the signature, so only a
    ``{`` or ``;`` at nesting depth zero decides. Double-quoted strings,
    character literals, and ``//``/``/* */`` comments are skipped *after* an
    ``fn`` token so a brace
    inside them cannot fake a body. No text before an ``fn`` token is ever
    used to skip it (a glob like ``src/*`` in a comment or string is an
    unbalanced ``/*`` that would hide every later function), so an ``fn``
    token inside a comment still scans forward. That is the fail-closed
    choice: it can only report a body (uncovered), never silence a
    measurable file.
    """
    total = len(source)
    pos = 0
    while True:
        idx = source.find("fn", pos)
        if idx == -1:
            return False
        before = source[idx - 1] if idx > 0 else " "
        after_pos = idx + 2
        after = source[after_pos] if after_pos < total else " "
        if (before.isalnum() or before == "_") or (after.isalnum() or after == "_"):
            pos = idx + 2
            continue
        depth_paren = 0
        depth_bracket = 0
        cursor = after_pos
        in_string = False
        while cursor < total:
            ch = source[cursor]
            if in_string:
                if ch == "\\":
                    cursor += 2
                    continue
                if ch == '"':
                    in_string = False
                cursor += 1
                continue
            if ch == '"':
                in_string = True
                cursor += 1
                continue
            if ch == "'":
                # Skip Rust char literals, but leave lifetimes (for example
                # `'a`) to the normal signature scan.
                char_end = cursor + 1
                if char_end < total and source[char_end] == "\\":
                    char_end += 2
                    if char_end < total and source[char_end - 1] == "u" and source[char_end] == "{":
                        close = source.find("}", char_end + 1)
                        char_end = close + 1 if close != -1 else total
                else:
                    char_end += 1
                if char_end < total and source[char_end] == "'":
                    cursor = char_end + 1
                    continue
            if ch == "/" and cursor + 1 < total and source[cursor + 1] == "/":
                line_end = source.find("\n", cursor + 2)
                cursor = total if line_end == -1 else line_end + 1
                continue
            if ch == "/" and cursor + 1 < total and source[cursor + 1] == "*":
                close = source.find("*/", cursor + 2)
                cursor = total if close == -1 else close + 2
                continue
            if ch == "(":
                depth_paren += 1
            elif ch == ")":
                if depth_paren > 0:
                    depth_paren -= 1
            elif ch == "[":
                depth_bracket += 1
            elif ch == "]":
                if depth_bracket > 0:
                    depth_bracket -= 1
            elif ch == "{" and depth_paren == 0 and depth_bracket == 0:
                return True
            elif ch == ";" and depth_paren == 0 and depth_bracket == 0:
                break
            cursor += 1
        pos = idx + 2


def has_instrumentable_function(path):
    """Whether a source file contains a function body the coverage report can map."""
    try:
        source = (ROOT / path).read_text()
    except OSError:
        # An unreadable changed source is not evidence that the gate may skip it.
        return True
    return _contains_fn_body(source)


def evaluate(hits, added):
    """(covered, coverable, uncovered-added-lines) over changed executable lines."""
    by_file = {}
    for (path, line), count in hits.items():
        by_file.setdefault(normalize(path), {})[line] = count
    covered, uncovered = 0, []
    for path, line in sorted(added):
        norm = normalize(path)
        file_cov = by_file.get(norm)
        if file_cov is None:
            # No lcov record for this file at all. A changed source file the
            # report must instrument counts as uncovered, so bin code or a new
            # file can never pass as "nothing to measure".
            if is_measurable_source(norm) and has_instrumentable_function(norm):
                uncovered.append((path, line))
            continue
        count = file_cov.get(line)
        if count is None:
            continue
        if count > 0:
            covered += 1
        else:
            uncovered.append((path, line))
    return covered, covered + len(uncovered), uncovered


def error(message):
    print(f"::error::{message}", flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lcov", required=True)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument("--head-ref", default="HEAD")
    parser.add_argument("--threshold", type=float, default=80.0)
    args = parser.parse_args(argv)
    if not 0 < args.threshold <= 100:
        parser.error("--threshold must be in (0, 100]")

    hits, read_error = read_lcov(args.lcov)
    if hits is None:
        error(f"patch coverage gate is blind ({read_error}); failing closed")
        return 1

    added, diff_error = added_lines(args.base_ref, args.head_ref)
    if added is None:
        error(f"patch coverage gate could not read the diff ({diff_error}); failing closed")
        return 1

    covered, coverable, uncovered = evaluate(hits, added)
    if not coverable:
        print("patch coverage gate: no coverable changed lines; nothing to measure")
        return 0
    pct = 100.0 * covered / coverable
    print(f"patch coverage gate: {covered}/{coverable} changed lines covered ({pct:.1f}%)")
    if pct >= args.threshold:
        return 0
    for path, line in uncovered[:MAX_ANNOTATIONS]:
        error(f"{path}:{line} changed but not covered by tests")
    if len(uncovered) > MAX_ANNOTATIONS:
        error(f"... and {len(uncovered) - MAX_ANNOTATIONS} more uncovered changed line(s)")
    error(f"patch coverage gate: {pct:.1f}% below the {args.threshold:g}% HARD threshold")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
