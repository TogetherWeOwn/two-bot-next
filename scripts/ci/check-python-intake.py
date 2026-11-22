"""Ratchet direct stdlib intake in Python scripts against the merge base."""

import argparse
import ast
from collections import Counter, defaultdict
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
INTAKE = "scripts/script_intake.py"
APIS = {"json.load", "json.loads", "json.JSONDecoder", "json.decoder.JSONDecoder", "json.*",
        "urllib.request.urlopen", "urllib.request.build_opener", "urllib.request.urlretrieve",
        "urllib.request.OpenerDirector", "urllib.request.install_opener", "urllib.request.*"}


def aliases(nodes):
    # Keep each import binding seen in the file. A local alias cannot hide
    # a raw stdlib binding used in another scope.
    bindings = defaultdict(set)
    for node in nodes:
        if isinstance(node, ast.Import):
            for name in node.names:
                root = name.name.partition(".")[0]
                bindings[name.asname or root].add(name.name if name.asname else root)
        elif isinstance(node, ast.ImportFrom) and not node.level and node.module:
            for name in node.names:
                if name.name != "*":
                    bindings[name.asname or name.name].add(f"{node.module}.{name.name}")
                    continue
                for api in APIS:
                    module, _, attr = api.rpartition(".")
                    if module == node.module and attr != "*":
                        bindings[attr].add(api)
    return bindings


def qualified(node, bindings):
    if isinstance(node, ast.Name):
        return bindings.get(node.id, {node.id})
    if isinstance(node, ast.Attribute):
        return {f"{owner}.{node.attr}" for owner in qualified(node.value, bindings)}
    return set()


def uses(source, path):
    tree = ast.parse(source, filename=path)
    nodes = list(ast.walk(tree))
    bindings = aliases(nodes)
    call_by_target = {node.func: node for node in nodes if isinstance(node, ast.Call)}
    for node in nodes:
        if isinstance(node, (ast.Name, ast.Attribute)) and isinstance(node.ctx, ast.Load):
            for api in sorted(qualified(node, bindings) & APIS):
                yield api, ast.dump(call_by_target.get(node, node)), node.lineno
        elif isinstance(node, ast.ImportFrom) and not node.level:
            for name in node.names:
                api = f"{node.module}.{name.name}"
                if api in APIS:
                    yield api, ast.dump(name), name.lineno
        elif (isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
              and node.func.id == "getattr" and len(node.args) in (2, 3)
              and isinstance(node.args[1], ast.Constant)):
            names = {f"{owner}.{node.args[1].value}" for owner in qualified(node.args[0], bindings)}
            for api in sorted(names & APIS):
                yield api, ast.dump(call_by_target.get(node, node)), node.lineno


def additions(before, after, path):
    backlog = Counter((api, shape) for api, shape, _ in uses(before, path))
    for api, shape, line in sorted(uses(after, path), key=lambda use: use[2]):
        if backlog[api, shape]:
            backlog[api, shape] -= 1
        else:
            yield line, api


def git(root, *args):
    return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.PIPE)


def scanned(path):
    return (path != INTAKE and path.startswith(("scripts/", ".github/scripts/"))
            and path.endswith(".py") and not Path(path).name.startswith(("test_", "test-")))


def check(root, base_ref):
    base = git(root, "rev-parse", "--verify", "--end-of-options", f"{base_ref}^{{commit}}").decode().strip()
    base = git(root, "merge-base", base, "HEAD").decode().strip()
    changes = git(root, "diff", "--find-renames", "--name-status", "--diff-filter=AMRT",
                  "-z", base, "HEAD", "--", "scripts/", ".github/scripts/")
    fields = iter(changes.decode("utf-8", "surrogateescape").split("\0")[:-1])
    for status in fields:
        old_path = next(fields)
        path = next(fields) if status.startswith("R") else old_path
        if not scanned(path):
            continue
        before = b"" if status == "A" or not scanned(old_path) else git(root, "show", f"{base}:{old_path}")
        after = git(root, "show", f"HEAD:{path}")
        for line, api in additions(before, after, path):
            yield path, line, api


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("--hard", action="store_true")
    args = parser.parse_args()
    try:
        violations = list(check(ROOT, args.base))
    except (OSError, subprocess.CalledProcessError, SyntaxError, ValueError, RuntimeError):
        print("Python intake: could not inspect both Git revisions", file=sys.stderr)
        return args.hard
    for path, line, api in violations:
        print(f"{path!r}:{line}: use {INTAKE} instead of {api}", file=sys.stderr)
    return args.hard and bool(violations)


if __name__ == "__main__":
    sys.exit(main())
