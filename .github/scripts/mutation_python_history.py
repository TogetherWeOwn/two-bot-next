"""Prepare enough exact PR history for the Python mutation planner."""

import os
import re
import subprocess

from mutation_scope import ROOT, git, main as plan

HISTORY_DEPTH = 32


def proven_shallow_base(root, base, head):
    result = subprocess.run(["git", "merge-base", "--all", base, head], cwd=root,
                            stdout=subprocess.PIPE, text=True, timeout=30)
    if result.returncode == 1:
        return False
    result.check_returncode()
    candidates = result.stdout.splitlines()
    if len(candidates) != 1:
        return False
    # A shallow boundary outside the candidate's ancestry may conceal a
    # better (or a second incomparable) merge base. Never guess past it.
    shallow_path = git(root, "rev-parse", "--git-path", "shallow").strip()
    boundaries = (root / shallow_path).read_text(encoding="ascii").splitlines()
    outside = git(root, "rev-list", base, head, "--not", candidates[0]).splitlines()
    return set(outside).isdisjoint(boundaries)


def prepare_history(root, base, head):
    if not all(re.fullmatch(r"[0-9a-f]{40}", sha) for sha in (base, head)):
        raise ValueError("PR base and head must be full commit SHAs")
    if git(root, "rev-parse", "HEAD").strip() != head:
        raise ValueError("checkout differs from PR head")
    fetch = ["git", "fetch", "--no-tags", "--no-recurse-submodules"]
    tips = ["--", "origin", base, head]
    subprocess.run([*fetch, f"--depth={HISTORY_DEPTH}", *tips], cwd=root, check=True)
    if git(root, "rev-parse", "--is-shallow-repository").strip() == "false":
        return
    if proven_shallow_base(root, base, head):
        print(f"Python mutation history: exact merge base within depth {HISTORY_DEPTH}")
        return
    print("Python mutation history: fetching complete PR ancestry", flush=True)
    subprocess.run([*fetch, "--unshallow", *tips], cwd=root, check=True)
    if git(root, "rev-parse", "--is-shallow-repository").strip() != "false":
        raise ValueError("PR ancestry remains shallow after full fetch")


if __name__ == "__main__":
    prepare_history(ROOT, os.environ["BASE_SHA"], os.environ["HEAD_SHA"])
    plan()
