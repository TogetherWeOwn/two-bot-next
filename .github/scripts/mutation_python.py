"""Mutate supported Python operators on added lines and run offline unittest targets."""

import argparse
import ast
import json
import os
import signal
import subprocess
import sys
import tarfile
import tempfile
import tokenize
import unittest
from collections import Counter
from io import BytesIO
from pathlib import Path, PurePosixPath

from mutation_rust import verdict
from mutation_scope import added_lines, python_lines

# Bind only stdlib suites verified to exercise the script with offline fixtures.
SUITES = {
    "scripts/container-inputs.py": ("scripts/test_container_inputs.py",),
    "scripts/job-inputs.py": ("scripts/test_job_inputs.py",),
    "scripts/ci/check-floor-guard.py": (
        "scripts/ci/test_check_floor_guard.py", "scripts/ci/test_check_floor_guard_evasions_acceptance.py",
    ),
    "scripts/ci/check-patch-coverage.py": (
        "scripts/ci/test_check_patch_coverage.py", "scripts/ci/test_check_patch_coverage_quoted_paths_acceptance.py",
        "scripts/ci/test_check_patch_coverage_spaced_paths.py",
    ),
    "scripts/ci/check-semgrep-warn.py": ("scripts/ci/test_check_semgrep_warn.py",),
    "scripts/ci/check-squawk-migrations.py": ("scripts/ci/test_check_squawk_migrations.py",),
    "scripts/ci/check-test-inventory.py": ("scripts/ci/test_check_test_inventory.py",),
    "scripts/ci/check-unwrap-request-paths.py": ("scripts/ci/test_check_unwrap_request_paths.py",),
}
MAX_MUTANTS = 80
REPLACEMENTS = {b"==": b"!=", b"!=": b"==", b">": b">=", b">=": b">", b"<": b"<=",
                b"<=": b"<", b"and": b"or", b"or": b"and", b"+": b"-", b"-": b"+"}


def edits(source, changed):
    encoding, _ = tokenize.detect_encoding(BytesIO(source).readline)
    if encoding != "utf-8" or b"\r" in source.replace(b"\r\n", b""):
        return []
    tree = ast.parse(source)
    lines = source.splitlines(keepends=True)
    replacements = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Constant) and type(node.value) is bool and node.lineno in changed:
            replacements.add((node.lineno, node.col_offset, node.end_col_offset,
                              b"False" if node.value else b"True"))
        elif isinstance(node, (ast.Compare, ast.BoolOp, ast.BinOp)):
            if isinstance(node, ast.Compare):
                operands = [node.left, *node.comparators]
            elif isinstance(node, ast.BoolOp):
                operands = node.values
            else:
                operands = (node.left, node.right)
            for left, right in zip(operands, operands[1:]):
                line, start, end = left.end_lineno, left.end_col_offset, right.col_offset
                if line != right.lineno or line not in changed:
                    continue
                operator = lines[line - 1][start:end].strip()
                if operator in REPLACEMENTS:
                    replacements.add((line, start, end, b" " + REPLACEMENTS[operator] + b" "))
    return sorted(replacements)


def run_tests(root, suites):
    command = [sys.executable, "-B", str(Path(__file__).resolve()), "--tests", *suites]
    with subprocess.Popen(command, cwd=root, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                          start_new_session=True) as process:
        try:
            return process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
            return 2


def unittest_exit(paths):
    suite = unittest.TestSuite()
    for path in map(PurePosixPath, paths):
        suite.addTests(unittest.TestLoader().discover(str(path.parent), pattern=path.name))
    result = unittest.TextTestRunner(verbosity=0).run(suite)
    if not result.wasSuccessful():
        return 1
    return 0 if result.testsRun > len(result.skipped) else 5


def measure(root, files, bindings=SUITES):
    results, plans = [], []
    for relative, changed in sorted(files.items()):
        file = root / relative
        suites = bindings.get(relative)
        if not suites or file.resolve() != file or not file.is_file():
            results.append({"file": relative, "status": "unsupported", "reason": "no offline suite or regular source"})
            continue
        source = file.read_bytes()
        try:
            candidates = edits(source, changed)
        except UnicodeError:
            candidates = []
        if not candidates:
            results.append({"file": relative, "status": "unsupported", "reason": "no supported added operator"})
            continue
        plans.append((file, suites, source, candidates))
    if sum(len(candidates) for _, _, _, candidates in plans) > MAX_MUTANTS:
        return [{"status": "infrastructure_error", "reason": "Python candidate budget exceeded"}]
    for file, suites, source, candidates in plans:
        relative = file.relative_to(root).as_posix()
        baseline = run_tests(root, suites)
        if baseline:
            results.append({"file": relative, "status": "unsupported" if baseline == 5 else "infrastructure_error",
                            "reason": "baseline ran zero tests" if baseline == 5 else "unmutated suite failed"})
            continue
        lines = source.splitlines(keepends=True)
        for line, start, end, replacement in candidates:
            row = lines[line - 1]
            mutant = row[:start] + replacement + row[end:]
            try:
                file.write_bytes(b"".join([*lines[:line - 1], mutant, *lines[line:]]))
                result = run_tests(root, suites)
            finally:
                file.write_bytes(source)
            outcome = {0: "survived", 1: "killed"}.get(result, "infrastructure_error")
            results.append({"file": relative, "line": line, "byte_offset": start, "status": outcome})
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tests", nargs="+")
    parser.add_argument("--mode", choices=("warn", "hard"), default="warn")
    parser.add_argument("--patch", type=Path)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if os.environ.get("GITHUB_ACTIONS") != "true":
        parser.error("mutation execution is limited to GitHub Actions")
    if args.tests:
        return unittest_exit(args.tests)
    if not args.patch or not args.report:
        parser.error("measurement requires --patch and --report")
    try:
        files = python_lines(added_lines(args.patch.read_text(encoding="utf-8", errors="surrogateescape")))
        if not files:
            raise ValueError("measurement has no Python additions")
        with tempfile.TemporaryDirectory(dir=os.environ["RUNNER_TEMP"]) as temporary:
            archive = Path(temporary, "head.tar")
            root = Path(temporary, "head")
            subprocess.run(["git", "archive", "-o", str(archive), "HEAD"], check=True, timeout=60)
            with tarfile.open(archive) as contents:
                contents.extractall(root, filter="data")
            results = measure(root, files)
    except (OSError, SyntaxError, ValueError, subprocess.SubprocessError, tarfile.TarError):
        results = [{"status": "infrastructure_error", "reason": "Python scope, engine, or report failed"}]
    args.report.write_text(json.dumps({"results": results}, indent=2) + "\n", encoding="utf-8")
    counts = Counter(row["status"] for row in results)
    print("Python mutation " + " ".join(f"{name}={counts[name]}" for name in
          ("killed", "survived", "unsupported", "infrastructure_error")))
    if counts["survived"] or counts["infrastructure_error"]:
        print("::warning::Python mutation found survivors or incomplete measurements; see the receipt")
    return verdict({"results": results}, args.mode)


if __name__ == "__main__":
    sys.exit(main())
