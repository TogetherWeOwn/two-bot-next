"""Plan mutation testing from the exact PR head and its merge base."""

import json
import os
import re
import runpy
import subprocess
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SOURCE = re.compile(r"(?:src/|crates/[^/]+/src/).+\.rs\Z")
HUNK = re.compile(r"@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")
parse_path = runpy.run_path(str(ROOT / "scripts/ci/check-patch-coverage.py"))["_parse_new_file_header"]
selector = runpy.run_path(str(ROOT / "scripts/job-inputs.py"))


def git(root, *args):
    return subprocess.check_output(["git", *args], cwd=root, timeout=30).decode(
        "utf-8", errors="surrogateescape"
    )


def pr_diff(root, base, head):
    if not all(re.fullmatch(r"[0-9a-f]{40}", sha) for sha in (base, head)):
        raise ValueError("PR base and head must be full commit SHAs")
    if git(root, "rev-parse", "HEAD").strip() != head:
        raise ValueError("checkout differs from PR head")
    merge_base = git(root, "merge-base", base, head).strip()
    return merge_base, git(
        root, "-c", "core.quotePath=true", "diff", "--no-color", "--no-ext-diff",
        "--no-textconv", "--find-renames", "--src-prefix=a/", "--dst-prefix=b/",
        "--text", "--unified=0", merge_base, head, "--", "*.rs", "*.py", "*.ts",
    )


def added_lines(patch):
    added = defaultdict(set)
    path = None
    in_hunk = False
    for raw in patch.split("\n"):
        if raw.startswith("diff --git "):
            path, in_hunk = None, False
        elif not in_hunk and raw.startswith("+++ "):
            path = parse_path(raw)
        elif raw.startswith("@@ "):
            match = HUNK.match(raw)
            if not match:
                raise ValueError("invalid unified diff hunk")
            in_hunk = True
            if path is not None and not path.startswith(("/", "../")):
                start = int(match[1])
                count = int(match[2]) if match[2] is not None else 1
                if count:
                    added[path].update(range(start, start + count))
    return dict(added)


def rust_lines(added):
    return {path: lines for path, lines in added.items() if SOURCE.fullmatch(path)}


def python_lines(added):
    return {path: lines for path, lines in added.items()
            if path.startswith("scripts/") and path.endswith(".py")
            and not any(part in ("tests", "fixtures") for part in Path(path).parts)
            and not Path(path).name.startswith("test_") and not Path(path).name.startswith("test-")}


def worker_lines(added):
    return {path: sorted(lines) for path, lines in added.items()
            if path.startswith("wrangler/src/") and path.endswith(".ts") and not path.endswith(".d.ts")}


def main():
    head = os.environ["HEAD_SHA"]
    base, patch = pr_diff(ROOT, os.environ["BASE_SHA"], head)
    paths = selector["changed_files"](base, head)
    jobs = selector["selection"](paths)
    lines = added_lines(patch)
    rust, python = bool(rust_lines(lines)), bool(python_lines(lines))
    worker_paths = worker_lines(lines)
    if (rust or python) and not jobs["rust"]:
        raise ValueError("Rust or Python additions disagree with job-inputs")
    if worker_paths and not jobs["worker"]:
        raise ValueError("Worker additions disagree with job-inputs")
    common = {".gitattributes", ".github/scripts/mutation_scope.py", ".github/workflows/mutation.yml"}
    fixture = rust or any(path in common or path.startswith(".github/scripts/mutation_rust") for path in paths)
    python_fixture = python or any(path in common or path.startswith(".github/scripts/mutation_python") for path in paths)
    worker_fixture = bool(worker_paths) or any(path in common or path.startswith(".github/mutation-worker/") for path in paths)
    temporary = Path(os.environ["RUNNER_TEMP"])
    (temporary / "mutation.diff").write_bytes(patch.encode("utf-8", "surrogateescape"))
    (temporary / "worker-mutation-plan.json").write_text(json.dumps(worker_paths), encoding="utf-8")
    print(f"Mutation scope HEAD {head} merge base {base}")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        for name, selected in (("rust", rust), ("fixture", fixture), ("python", python),
                               ("python_fixture", python_fixture), ("worker", bool(worker_paths)),
                               ("worker_fixture", worker_fixture)):
            print(f"{name}={str(selected).lower()}", file=output)


if __name__ == "__main__":
    main()
