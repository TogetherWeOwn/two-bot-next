"""Ratchet direct stdlib intake in Python scripts against the merge base."""

import argparse
import ast
from collections import Counter
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
INTAKE = "scripts/script_intake.py"
APIS = {"json.load", "json.loads", "json.JSONDecoder", "json.decoder.JSONDecoder", "json.*",
        "urllib.request.urlopen", "urllib.request.build_opener", "urllib.request.urlretrieve",
        "urllib.request.OpenerDirector", "urllib.request.install_opener", "urllib.request.*"}


def aliases(nodes):
    bindings = {}
    for node in nodes:
        if isinstance(node, ast.Import):
            for name in node.names:
                root = name.name.split(".")[0]
                bindings[name.asname or root] = name.name if name.asname else root
        elif isinstance(node, ast.ImportFrom) and not node.level and node.module:
            for name in node.names:
                if name.name == "*":
                    for api in APIS:
                        module, _, attr = api.rpartition(".")
                        if module == node.module and attr != "*":
                            bindings.setdefault(attr, api)
                else:
                    bindings[name.asname or name.name] = f"{node.module}.{name.name}"
    return bindings


def qualified(node, bindings):
    if isinstance(node, ast.Name):
        return bindings.get(node.id, node.id)
    if isinstance(node, ast.Attribute):
        owner = qualified(node.value, bindings)
        if owner is not None:
            return f"{owner}.{node.attr}"
    return None


def uses(source, path):
    tree = ast.parse(source, filename=path)
    nodes = list(ast.walk(tree))
    bindings = aliases(nodes)
    call_by_target = {node.func: node for node in nodes if isinstance(node, ast.Call)}
    for node in nodes:
        if isinstance(node, (ast.Name, ast.Attribute)) and isinstance(node.ctx, ast.Load):
            api = qualified(node, bindings)
            if api in APIS:
                yield api, ast.dump(call_by_target.get(node, node)), node.lineno
        elif isinstance(node, ast.ImportFrom) and not node.level:
            for name in node.names:
                api = f"{node.module}.{name.name}"
                if api in APIS:
                    yield api, ast.dump(name), name.lineno
        elif (isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
              and node.func.id == "getattr" and len(node.args) in (2, 3)
              and isinstance(node.args[1], ast.Constant)):
            api = f"{qualified(node.args[0], bindings)}.{node.args[1].value}"
            if api in APIS:
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


def check(root, base_ref, head_ref):
    head = git(root, "rev-parse", "--verify", "--end-of-options", f"{head_ref}^{{commit}}").strip().decode()
    base = git(root, "rev-parse", "--verify", "--end-of-options", f"{base_ref}^{{commit}}").strip().decode()
    base = git(root, "merge-base", base, head).strip().decode()
    changes = git(root, "diff", "--no-renames", "--name-status", "--diff-filter=AMT",
                  "-z", base, head, "--", "scripts/", ".github/scripts/")
    fields = changes.decode("utf-8", "surrogateescape").split("\0")[:-1]
    for status, path in zip(fields[::2], fields[1::2], strict=True):
        if path == INTAKE or not path.endswith(".py") or Path(path).name.startswith(("test_", "test-")):
            continue
        before = b"" if status == "A" else git(root, "show", f"{base}:{path}")
        after = git(root, "show", f"{head}:{path}")
        for line, api in additions(before, after, path):
            yield path, line, api


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    args = parser.parse_args()
    try:
        violations = list(check(ROOT, args.base, "HEAD"))
    except (OSError, subprocess.CalledProcessError, SyntaxError, ValueError):
        print("Python intake: could not inspect both Git revisions", file=sys.stderr)
        return 1
    for path, line, api in violations:
        print(f"{path!r}:{line}: use {INTAKE} instead of {api}", file=sys.stderr)
    return bool(violations)


if __name__ == "__main__":
    sys.exit(main())
