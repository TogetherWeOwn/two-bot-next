"""Run only cargo-mutants edits contained in added PR lines."""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path

from mutation_scope import added_lines, rust_lines

MAX_MUTANTS = 100
OUTCOMES = {"CaughtMutant": "killed", "MissedMutant": "survived",
            "Unviable": "unsupported", "Timeout": "infrastructure_error"}


def location(mutant):
    start, end = (mutant["span"][key] for key in ("start", "end"))
    first = start["line"]
    last = end["line"] - (end["column"] == 1 and end["line"] > first)
    return mutant["file"], first, last


def in_scope(mutant, added):
    path, first, last = location(mutant)
    return first > 0 and last >= first and all(
        line in added.get(path, ()) for line in range(first, last + 1)
    )


def exact_regex(name):
    escaped = re.sub(r"([\\.+*?()\[\]{}|^$])", r"\\\1", name)
    return r"\A" + escaped + r"\z"


def measure(root, patch, output):
    added = rust_lines(added_lines(patch.read_text(encoding="utf-8", errors="surrogateescape")))
    if not added:
        raise ValueError("Rust measurement was selected without source additions")
    command = ["cargo", "mutants", "--workspace", "--no-config", "--in-diff", str(patch),
               "--all-features", "--gitignore=false", "--cargo-arg=--locked",
               "--build-timeout=720", "--timeout=90"]
    discovery = subprocess.run(command + ["--list", "--json"], cwd=root, capture_output=True,
                               check=True, timeout=120)
    # cargo-mutants exits 0 without writing a JSON array when the diff matches nothing.
    listed = json.loads(discovery.stdout or b"[]")
    selected = [mutant for mutant in listed if in_scope(mutant, added)]
    omitted = len(listed) - len(selected)
    if not selected:
        return {"status": "unsupported", "out_of_scope": omitted, "results": []}
    if len(selected) > MAX_MUTANTS:
        raise ValueError(f"Rust PR exceeds the {MAX_MUTANTS}-mutant count budget")
    expected = {mutant["name"] for mutant in selected}
    if len(expected) != len(selected):
        raise ValueError("Rust discovery returned duplicate mutation identities")
    filters = [f"--re={exact_regex(name)}" for name in sorted(expected)]
    # The standard package tests run serially to avoid process-global races.
    run = subprocess.run(command + filters + ["--output", str(output)],
                         cwd=root, env={**os.environ, "RUST_TEST_THREADS": "1"},
                         capture_output=True, timeout=32 * 60)
    if run.returncode not in (0, 2, 3):
        raise ValueError(f"cargo-mutants failed before a complete verdict (exit {run.returncode})")
    report = json.loads((output / "mutants.out/outcomes.json").read_bytes())
    baseline, *mutations = report["outcomes"]
    if baseline["scenario"] != "Baseline" or baseline["summary"] != "Success":
        raise ValueError("Rust baseline did not pass")
    results = []
    for outcome in mutations:
        mutant = outcome["scenario"]["Mutant"]
        if not in_scope(mutant, added) or mutant["name"] not in expected:
            raise ValueError("unexpected or out-of-scope Rust mutation")
        expected.remove(mutant["name"])
        path, line, _ = location(mutant)
        identity = hashlib.sha256(mutant["name"].encode()).hexdigest()
        results.append({"file": path, "line": line, "genre": mutant["genre"], "id": identity,
                        "status": OUTCOMES[outcome["summary"]]})
    if expected:
        raise ValueError("Rust report is missing selected mutations")
    return {"status": "measured", "out_of_scope": omitted, "results": results}


def verdict(report, mode):
    if mode == "warn":
        return 0
    counts = Counter(result["status"] for result in report["results"])
    if report.get("status") == "infrastructure_error" or counts["infrastructure_error"]:
        return 2
    return int(counts["survived"] > 0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("warn", "hard"), required=True)
    parser.add_argument("--patch", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if os.environ.get("GITHUB_ACTIONS") != "true":
        parser.error("mutation execution is limited to GitHub Actions")
    try:
        with tempfile.TemporaryDirectory(dir=os.environ["RUNNER_TEMP"]) as output:
            report = measure(Path.cwd(), args.patch.resolve(), Path(output))
    except ValueError as error:
        # Do not publish tool output, mutant source text or test logs.
        report = {"status": "infrastructure_error", "reason": str(error), "results": []}
    except (KeyError, TypeError, OSError, subprocess.SubprocessError):
        report = {"status": "infrastructure_error", "reason": "Rust engine or report failed", "results": []}
    args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    counts = Counter(result["status"] for result in report["results"])
    print(f"Rust mutation status={report['status']} " + " ".join(
        f"{status}={counts[status]}" for status in OUTCOMES.values()
    ))
    if report["status"] == "infrastructure_error" or counts["infrastructure_error"]:
        print("::warning::Rust mutation measurement is incomplete; see the receipt")
    if counts["survived"]:
        print("::warning::Rust mutations survived on added PR lines; see the receipt")
    return verdict(report, args.mode)


if __name__ == "__main__":
    sys.exit(main())
