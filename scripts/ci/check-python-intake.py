"""Ratchet direct stdlib intake in Python scripts against the merge base."""

import argparse
import ast
from collections import Counter, defaultdict
from copy import deepcopy
import os
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
INTAKE = "scripts/script_intake.py"
APIS = {"json.load", "json.loads", "json.JSONDecoder", "json.decoder.JSONDecoder", "json.*",
        "urllib.request.urlopen", "urllib.request.build_opener", "urllib.request.urlretrieve",
        "urllib.request.OpenerDirector", "urllib.request.install_opener", "urllib.request.*"}
FUNCTION_SCOPES = (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda)
OTHER_SCOPES = (ast.ClassDef, ast.ListComp, ast.SetComp, ast.DictComp, ast.GeneratorExp)


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


def function_locals(nodes):
    scopes = {}
    bound = defaultdict(set)
    external = defaultdict(set)
    for node in nodes:
        scope = scopes.get(node)
        if scope is not None:
            if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Store):
                bound[scope].add(node.id)
            elif isinstance(node, ast.arg):
                bound[scope].add(node.arg)
            elif isinstance(node, (ast.Global, ast.Nonlocal)):
                external[scope].update(node.names)
        if isinstance(node, FUNCTION_SCOPES):
            body = {node.body} if isinstance(node, ast.Lambda) else set(node.body)
            body.add(node.args)
            scopes.update((child, node if child in body else scope) for child in ast.iter_child_nodes(node))
        elif not isinstance(node, OTHER_SCOPES):
            scopes.update((child, scope) for child in ast.iter_child_nodes(node))
    return scopes, {scope: names - external[scope] for scope, names in bound.items()}


def call_shape(node, locals_in_scope):
    """Keep argument structure and literals while ignoring local spelling."""
    if not locals_in_scope:
        return ast.dump(node)
    clone = deepcopy(node)
    names = {}
    for reference in ast.walk(clone):
        if isinstance(reference, ast.Name) and reference.id in locals_in_scope:
            index = names.setdefault(reference.id, len(names))
            reference.id = f"<local {index}>"
    return ast.dump(clone)


def uses(source, path):
    tree = ast.parse(source, filename=path)
    nodes = list(ast.walk(tree))
    bindings = aliases(nodes)
    call_by_target = {node.func: node for node in nodes if isinstance(node, ast.Call)}
    scopes, locals_by_scope = function_locals(nodes)

    def shape(target):
        node = call_by_target.get(target, target)
        return call_shape(node, locals_by_scope.get(scopes.get(node), ()))

    for node in nodes:
        if isinstance(node, (ast.Name, ast.Attribute)) and isinstance(node.ctx, ast.Load):
            for api in sorted(qualified(node, bindings) & APIS):
                yield api, shape(node), node.lineno
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
                yield api, shape(node), node.lineno


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
    changes = git(root, "diff", "--find-renames", "--raw", "--no-abbrev", "--diff-filter=AMRT",
                  "-z", base, "HEAD", "--", "scripts/", ".github/scripts/")
    fields = iter(changes.decode("utf-8", "surrogateescape").split("\0")[:-1])
    regular_modes = {"100644", "100755"}
    for header in fields:
        old_mode, new_mode, old_blob, new_blob, status = header.removeprefix(":").split()
        old_path = next(fields)
        path = next(fields) if status.startswith("R") else old_path
        if not scanned(path):
            continue
        if new_mode not in regular_modes:
            yield path, 1, None
            continue
        try:
            before = b""
            if old_mode in regular_modes and scanned(old_path):
                before = git(root, "cat-file", "blob", old_blob)
            after = git(root, "cat-file", "blob", new_blob)
            for line, api in additions(before, after, path):
                yield path, line, api
        except (OSError, subprocess.CalledProcessError, SyntaxError, ValueError, RecursionError):
            yield path, 1, None


def report(message, *, hard, path=None, line=None):
    if os.environ.get("GITHUB_ACTIONS") != "true":
        prefix = "" if path is None else f"{path!r}:{line}: "
        print(prefix + message, file=sys.stderr)
        return
    level = "error" if hard else "warning"
    if path is None:
        print(f"::{level}::{message}")
        return
    path = "".join(c if c.isprintable() or c in "\r\n" else ascii(c)[1:-1] for c in path)
    for raw, escaped in (("%", "%25"), ("\r", "%0D"), ("\n", "%0A"),
                         (":", "%3A"), (",", "%2C")):
        path = path.replace(raw, escaped)
    print(f"::{level} file={path},line={line}::{message}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("--hard", action="store_true")
    args = parser.parse_args()
    try:
        violations = list(check(ROOT, args.base))
    except (OSError, subprocess.CalledProcessError, SyntaxError, ValueError, RuntimeError):
        report("Python intake: could not inspect both Git revisions", hard=args.hard)
        return args.hard
    for path, line, api in violations:
        message = "Python intake: could not scan source" if api is None else f"use {INTAKE} instead of {api}"
        report(message, hard=args.hard, path=path, line=line)
    return args.hard and bool(violations)


if __name__ == "__main__":
    sys.exit(main())
