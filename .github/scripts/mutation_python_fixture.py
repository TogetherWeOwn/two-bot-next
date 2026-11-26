"""Actions-only fixtures for Python mutation scope and unittest verdicts."""

import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from unittest.mock import patch as mock_patch

import yaml

from mutation_python import RUN_SECONDS, measure, snapshot, verdict
from mutation_python_history import prepare_history
from mutation_rust_fixture import commit
from mutation_scope import ROOT, added_lines, git, pr_diff, python_lines

MODULE = "scripts/rule.py"
TESTS = "scripts/test_rule.py"
OLD = "def legacy(n): return n > 99\ndef eligible(n):\n    return ((n)  # Compare the input\n        > 3)\n"
NEW = OLD.replace("> 3", ">= 3")
HEADER = "import unittest\nimport yaml\nfrom rule import eligible, legacy\n\nclass RuleTests(unittest.TestCase):\n"


def test_exact_checkout_history(scratch):
    upstream = scratch / "upstream"
    (upstream / "scripts").mkdir(parents=True)
    git(upstream, "init", "-q")
    module = upstream / MODULE
    module.write_text(OLD)
    seed = commit(upstream, "seed")
    old_tree = git(upstream, "rev-parse", "HEAD^{tree}").strip()
    module.write_text(NEW)
    commit(upstream, "changed operator")
    new_tree = git(upstream, "rev-parse", "HEAD^{tree}").strip()

    def node(message, tree, *parents):
        return git(upstream, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                   "commit-tree", tree, "-m", message,
                   *(arg for parent in parents for arg in ("-p", parent))).strip()

    old = node("old", old_tree, seed)
    common = node("common", old_tree, old)
    fast_head = node("changed operator on short history", new_tree, common)
    # Both tips reach old within depth three. Only the full graph shows
    # that the head also reaches the newer common commit via the long arm.
    base = node("base", old_tree, common)
    side = node("side", old_tree, old)
    long_arm = node("second on long arm", old_tree, node("first on long arm", old_tree, common))
    head = node("changed operator after merge", new_tree, side, long_arm)
    deep_base = node("deep base", old_tree, base)
    deep_head = node("deep head with no shallow base", new_tree, long_arm)
    cross_base = node("base with two common ancestors", old_tree, common, side)
    cross_head = node("head with two common ancestors", new_tree, side, common)
    for label, base_sha, head_sha, shallow in (
        ("bounded", common, fast_head, "true"),
        ("fallback", base, head, "false"),
        ("no-shallow-base", deep_base, deep_head, "false"),
        ("multiple-bases", cross_base, cross_head, "false"),
    ):
        git(upstream, "update-ref", f"refs/heads/{label}-head", head_sha)
        git(upstream, "update-ref", f"refs/heads/{label}-base", base_sha)
        checkout = scratch / label
        checkout.mkdir()
        git(checkout, "init", "-q")
        git(checkout, "remote", "add", "origin", upstream.as_uri())
        git(checkout, "fetch", "--no-tags", "--depth=1", "origin", head_sha)
        git(checkout, "checkout", "--detach", head_sha)
        with mock_patch("mutation_python_history.HISTORY_DEPTH", 3):
            prepare_history(checkout, base_sha, head_sha)
        assert git(checkout, "rev-parse", "--is-shallow-repository").strip() == shallow
        merge_base, diff = pr_diff(checkout, base_sha, head_sha)
        assert merge_base == git(upstream, "merge-base", base_sha, head_sha).strip(), merge_base
        assert python_lines(added_lines(diff)) == {MODULE: {4}}
        assert git(checkout, "rev-parse", "HEAD^{tree}").strip() == new_tree
        assert git(checkout, "archive", "HEAD") == git(upstream, "archive", head_sha)


def test_legacy_checkout_plan(scratch, workflow=ROOT / ".github/workflows/mutation.yml"):
    upstream = scratch / "legacy-source"
    for path in (".github/scripts/mutation_scope.py", "scripts/job-inputs.py",
                 "scripts/ci/check-patch-coverage.py"):
        target = upstream / path
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / path, target)
    git(upstream, "init", "-q")
    module = upstream / MODULE
    module.write_text(OLD)
    base = commit(upstream, "legacy base")
    module.write_text(NEW)
    head = commit(upstream, "legacy head with Python additions")
    checkout = scratch / "legacy-checkout"
    git(scratch, "clone", "--quiet", "--no-tags", "--depth=1", upstream.as_uri(), str(checkout))
    git(checkout, "checkout", "--detach", head)

    python_job = yaml.load(workflow.read_text(), Loader=yaml.BaseLoader)["jobs"]["python-mutation"]
    plan = next(step["run"] for step in python_job["steps"] if step.get("id") == "plan")
    outputs = scratch / "legacy-outputs"
    env = dict(os.environ, BASE_SHA=base, HEAD_SHA=head,
               RUNNER_TEMP=str(scratch), GITHUB_OUTPUT=str(outputs))
    result = subprocess.run(["bash", "-e", "-c", plan], cwd=checkout, env=env, check=True,
                            stdout=subprocess.PIPE, text=True, timeout=30)
    assert f"Mutation scope HEAD {head} merge base {base}" in result.stdout
    assert outputs.read_text().splitlines() == [
        "rust=false", "fixture=false", "python=true", "python_fixture=true",
        "worker=false", "worker_fixture=false",
    ]
    diff = (scratch / "mutation.diff").read_text()
    assert python_lines(added_lines(diff)) == {MODULE: {4}}
    assert git(checkout, "archive", "HEAD") == git(upstream, "archive", head)


def main():
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise SystemExit("mutation fixtures run only in GitHub Actions")
    with tempfile.TemporaryDirectory(dir=os.environ["RUNNER_TEMP"]) as scratch:
        test_exact_checkout_history(Path(scratch))
        test_legacy_checkout_plan(Path(scratch))
        root = Path(scratch, "repo")
        root.mkdir()
        git(root, "init", "-q")
        (root / "scripts").mkdir()
        module, tests = root / MODULE, root / TESTS
        module.write_text(OLD)
        base = commit(root, "baseline")
        module.write_text(NEW)
        for label, test, status, expected in (
            ("strong", "def test_boundary(self): self.assertTrue(eligible(yaml.safe_load('3')))", "killed", 0),
            ("weak", "def test_legacy(self): self.assertTrue(legacy(100))", "survived", 1),
            ("broken", "def test_broken(self): self.assertFalse(eligible(3))", "infrastructure_error", 2),
            ("empty", "pass", "unsupported", 0),
        ):
            tests.write_text(HEADER + "    " + test + "\n")
            head = commit(root, label)
            _, patch = pr_diff(root, base, head)
            changed = python_lines(added_lines(patch))
            assert changed == {MODULE: {4}}, changed
            with tempfile.TemporaryDirectory(dir=scratch) as temporary:
                copy = snapshot(root, Path(temporary))
                results = measure(copy, changed, {MODULE: (TESTS,)})
                if label == "strong":
                    # Expire the measurement budget after the real baseline passes.
                    with mock_patch("mutation_python.monotonic", side_effect=[0, 0, 0, RUN_SECONDS]):
                        expired = measure(copy, changed, {MODULE: (TESTS,)})
                    assert expired == [{"status": "infrastructure_error", "reason": "Python run budget exceeded"}]
                    print("Python fixture: budget expired after baseline, before mutation")
                assert (copy / MODULE).read_text() == NEW
            assert len(results) == 1 and results[0]["status"] == status, results
            if status in ("killed", "survived"):
                assert results[0]["line"] == 4, results
            assert verdict({"results": results}, "hard") == expected
            assert verdict({"results": results}, "warn") == 0
            print(f"Python fixture {label}: {status}, HARD exit {expected}")


if __name__ == "__main__":
    main()
