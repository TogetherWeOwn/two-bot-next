#!/usr/bin/env python3
"""HARD gate: every test target and fuzz target is built or run by some CI job.

A target that no workflow references is an orphan: it compiles (or rots)
without ever executing, so a regression hides until release. The gate fails
with the orphan list; add the target to a lane or record a reasoned exemption
in EXEMPT below.

Coverage rules, in order:
1. Integration targets (``crates/*/tests/*.rs``) whose file contains
   ``#[ignore]`` must have executable routing evidence in a ``run:`` context
   in ``check.yml`` or ``nightly.yml``: a ``--test <name>`` (or
   ``--test=<name>``) argument in a non-echo, non-comment shell command, or
   membership in an explicit ``for <var> in <list>`` loop whose body runs
   ``--test "$<var>"``. A name that appears only in display text (a step or
   job ``name:``), an ``env:`` value, an ``if:`` expression, a comment, an
   ``echo``/``printf`` line, or a ``--skip`` argument is a mention, not a
   route, and the target is orphaned.
2. Integration targets without ``#[ignore]`` are covered by the workspace
   ``--test '*'`` integration run or a connected partition runner whose
   executable Cargo metadata discovery and dispatch are structurally verified.
   Partitioned ignored targets require a verified package/target owner.
3. Every fuzz target in ``fuzz/Cargo.toml`` must have a matching
   ``fuzz_targets/<name>.rs`` and the ``fuzz-compile`` job must build them.
4. The workspace lib/bins unit run, the doc-test run and an ``--all-targets``
   build (examples) must each be present.

Detection is a deliberately narrow heuristic, not a shell or YAML
interpreter: it reads ``run:`` values (inline and block scalars, with
backslash continuations joined) and recognizes the command shapes this
repository's workflows use today, including wrapper commands such as
``cov_run ... llvm-cov ... --test <name>`` where ``cargo test`` is not the
leading words. It does not expand shell variables (other than the explicit
for-loop list in rule 1), follow arbitrary generated commands, or count a bare
test-name filter without ``--test`` as routing an ignored target. The one
recognized generated route is the checked-in partition runner, inspected as
Python AST without executing or importing it; its planner, workers, strict
finals and ci-ok dependencies must be connected. Other shapes need an explicit
``--test <name>`` command or a reasoned exemption.
"""

import argparse
import ast
import re
import shlex
import textwrap
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Targets no CI job may run, each with the reason it stays unrouted. A reason
# is required: an orphan without one is a regression, not an exemption.
EXEMPT = {
    # Manual regen: rewrites docs/commands.md and docs/configuration.md in the
    # checkout, so CI must never run it (ignored-db-runtime documents this).
    "reference_docs": "manual doc regen; rewrites committed files",
}

WORKSPACE_LIB_BINS = "cargo test --workspace --lib --bins"
WORKSPACE_DOC = "cargo test --workspace --doc"
FUZZ_BUILD = "cargo +nightly-2026-10-01 fuzz build"

_RUN_RE = re.compile(r"^(?P<prefix>[ ]*(?:-[ ]+)?)run:[ ]*(?P<value>.*)$")
_BLOCK_SCALAR_RE = re.compile(r"^[|>][+-]?[0-9]*$")
_SKIP_RE = re.compile(r"--skip(?:\s+|=)[\"']?[A-Za-z0-9_-]+")
_TEST_ARG_RE = re.compile(r"--test(?:\s+|=)[\"']?([A-Za-z0-9_-]+)")
_BLANKET_RE = re.compile(r"--test(?:\s+|=)[\"']?\*")
_LOOP_RE = re.compile(
    r"\bfor\s+(?P<var>[A-Za-z_][\w]*)\s+in\s+(?P<list>[^;\n]*?)\s*(?:;|\n)\s*do\b"
    r"(?P<body>.*?)\bdone\b",
    re.DOTALL,
)
# A --test argument only routes a Rust target when the same command invokes
# cargo (directly, or via the cov_run llvm-cov wrapper); a `node --test`
# script or a grep for the literal text must not count.
_INVOCATION_RE = re.compile(r"\b(?:cargo|llvm-cov)\b")


def extract_run_blocks(text):
    """Return the shell text of each ``run:`` value, one entry per step.

    Inline values contribute one line; block scalars (``|``, ``>-`` and
    friends) contribute their indented body. An empty value (such as the
    ``defaults.run`` mapping) is not a shell command and is skipped. Lines
    consumed by a block scalar are not rescanned, so a ``run:``-looking
    line inside a script body cannot start a phantom step.
    """
    lines = text.splitlines()
    blocks = []
    index = 0
    while index < len(lines):
        match = _RUN_RE.match(lines[index])
        index += 1
        if match is None:
            continue
        value = match.group("value").strip()
        marker = value.split("#", 1)[0].strip() if value else ""
        if marker and _BLOCK_SCALAR_RE.match(marker):
            prefix = match.group("prefix")
            # YAML requires block-scalar lines to be indented deeper than
            # the `run` key itself, not the `- ` dash: a later step key
            # (name:, env:, if:) sits at the key column and ends the script.
            parent_col = len(prefix)
            body = []
            while index < len(lines):
                line = lines[index]
                if not line.strip():
                    body.append("")
                    index += 1
                    continue
                indent = len(line) - len(line.lstrip(" "))
                if indent <= parent_col:
                    break
                body.append(line)
                index += 1
            if marker.startswith(">"):
                # Folded scalar: single newlines fold to spaces, so a
                # wrapped command is one logical shell line.
                folded, pending = [], ""
                for line in body:
                    if line.strip():
                        pending = f"{pending} {line.strip()}".strip()
                    elif pending:
                        folded.append(pending)
                        pending = ""
                if pending:
                    folded.append(pending)
                body = folded
            blocks.append(body)
        elif value:
            blocks.append([match.group("value")])
    return blocks


def _logical_lines(body):
    """Join backslash-continued shell lines into single logical lines."""
    logical = []
    pending = ""
    for line in body:
        stripped = line.rstrip()
        if stripped.endswith("\\") and not stripped.endswith("\\\\"):
            pending += stripped[:-1] + " "
            continue
        logical.append(pending + line)
        pending = ""
    if pending:
        logical.append(pending)
    return logical


def _strip_inline_comment(line):
    in_single = in_double = False
    escaped = False
    for position, char in enumerate(line):
        if escaped:
            escaped = False
            continue
        if char == "\\" and not in_single:
            escaped = True
            continue
        if char == "'" and not in_double:
            in_single = not in_single
        elif char == '"' and not in_single:
            in_double = not in_double
        elif char == "#" and not in_single and not in_double and (
                position == 0 or line[position - 1] in " \t;|&"):
            return line[:position]
    return line


def _split_segments(line):
    """Split a logical line on ``;``, ``&&``, ``||`` and ``|`` outside quotes."""
    segments = []
    start = 0
    depth = 0
    in_single = in_double = False
    escaped = False
    index = 0
    while index < len(line):
        char = line[index]
        if escaped:
            escaped = False
        elif char == "\\" and not in_single:
            escaped = True
        elif char == "'" and not in_double:
            in_single = not in_single
        elif char == '"' and not in_single:
            in_double = not in_double
        elif not in_single and not in_double:
            if char == "(":
                depth += 1
            elif char == ")":
                depth = max(0, depth - 1)
            elif depth == 0 and char == ";":
                segments.append(line[start:index])
                start = index + 1
            elif depth == 0 and line.startswith(("&&", "||"), index):
                segments.append(line[start:index])
                start = index + 2
                index += 1
            elif depth == 0 and char == "|":
                segments.append(line[start:index])
                start = index + 1
        index += 1
    segments.append(line[start:])
    return segments


_WRAPPER_WORDS = ("do", "then", "else", "time", "!", "sudo", "command",
                  "builtin", "env")
_TOKEN_RE = re.compile(r"^\s*(\S+)")


def _is_print_segment(segment):
    """True for echo/printf commands, which print a name without running it.

    Peel shell keywords, ``VAR=value`` prefixes and wrappers such as
    ``sudo``/``env`` (and their flags) so a wrapped or path-qualified
    ``echo`` still counts as a print, not a route.
    """
    rest = segment
    after_wrapper = False
    while True:
        match = _TOKEN_RE.match(rest)
        if match is None:
            return False
        token = match.group(1).lstrip("({!")
        rest = rest[match.end():]
        if token in _WRAPPER_WORDS:
            after_wrapper = token in ("sudo", "env")
            continue
        if re.match(r"[A-Za-z_][\w]*=", token):
            continue
        if after_wrapper and token.startswith("-"):
            continue
        return token.rsplit("/", 1)[-1] in ("echo", "printf")


def executable_block_text(body):
    """Shell text stripped of comments, echo/printf prints and --skip args."""
    kept = []
    for line in _logical_lines(body):
        line = _strip_inline_comment(line)
        for segment in _split_segments(line):
            if not _is_print_segment(segment):
                kept.append(_SKIP_RE.sub("", segment))
    return "\n".join(kept)


def _compile_only(segment):
    # Cargo flags end at `--`; libtest filters and comments must not suppress a run.
    try:
        words = shlex.split(segment)
    except ValueError:
        return True  # An unparseable command cannot prove execution.
    cargo_words = words[:words.index("--")] if "--" in words else words
    return "--no-run" in cargo_words


def routed_targets(*workflow_texts):
    """Return (executably routed target names, executable texts per workflow)."""
    routed = set()
    executable_texts = []
    for text in workflow_texts:
        blocks = [executable_block_text(body) for body in extract_run_blocks(text)]
        executable_texts.append("\n".join(blocks))
        for block in blocks:
            for segment in block.splitlines():
                if _compile_only(segment):
                    continue
                for arg in _TEST_ARG_RE.finditer(segment):
                    if _INVOCATION_RE.search(segment[:arg.start()]):
                        routed.add(arg.group(1))
            for loop in _LOOP_RE.finditer(block):
                var = re.escape(loop.group("var"))
                runs_loop_var = any(
                    (arg := re.search(
                        r"--test(?:\s+|=)[\"']?\$(?:\{%s\}|%s\b)" % (var, var),
                        line)) and _INVOCATION_RE.search(line[:arg.start()])
                    for line in loop.group("body").splitlines() if not _compile_only(line))
                if runs_loop_var:
                    routed.update(
                        token.strip("\"'")
                        for token in loop.group("list").split())
    return routed, executable_texts


def _jobs(text):
    """Read only the conventional, two-space-indented jobs used by check.yml.

    This deliberately does not interpret arbitrary YAML or shell. Unsupported
    shapes fail closed instead of turning a mention into execution evidence.
    """
    match = re.search(r"(?m)^jobs:\s*(?:#.*)?$", text)
    if not match:
        return {}
    lines = text[match.end():].splitlines()
    jobs = {}
    current = None
    for line in lines:
        if line and not line.startswith(" ") and not line.startswith("#"):
            break
        found = re.match(r"^  ([\w-]+):\s*(?:#.*)?$", line)
        if found:
            current = found.group(1)
            if current in jobs:
                return {}
            jobs[current] = []
        elif current is not None:
            jobs[current].append(line)
    return {name: "\n".join(body) for name, body in jobs.items()}


def _field(text, key, indent=4):
    matches = re.findall(r"(?m)^" + " " * indent + re.escape(key) + r":[ \t]*(.*?)[ \t]*$", text)
    return matches[0] if len(matches) == 1 else ""


def _needs(job):
    value = _field(job, "needs")
    if not (value.startswith("[") and value.endswith("]")):
        return set()
    return {item.strip().strip("'\"") for item in value[1:-1].split(",")}


def _steps(job):
    # A step begins at exactly six spaces; nested lists are not steps.
    return re.split(r"(?m)^      - ", job)[1:]


def _step_field(step, key):
    first = re.match(re.escape(key) + r":\s*(.*?)\s*(?:\n|$)", step)
    return first.group(1) if first else _field(step, key, 8)


def _step_env(job, step, key):
    def env_value(text, indent):
        lines = text.splitlines()
        starts = [i for i, line in enumerate(lines) if line == ' ' * indent + 'env:']
        if len(starts) != 1:
            return ''
        body = []
        for line in lines[starts[0] + 1:]:
            if line.strip() and len(line) - len(line.lstrip(' ')) <= indent:
                break
            body.append(line)
        return _field('\n'.join(body), key, indent + 2)
    return env_value(step, 8) or env_value(job, 4)


def _unconditional_step(step):
    return not _step_field(step, "if") and not _step_field(step, "continue-on-error")


def _direct_run(step, pattern):
    # Require a whole actual command, not an echo, argument, comment or --help.
    if not _unconditional_step(step):
        return False
    blocks = extract_run_blocks("      - " + step)
    lines = [line.strip() for body in blocks
             for line in executable_block_text(body).splitlines() if line.strip()]
    return len(blocks) == 1 and len(lines) == 1 and bool(re.fullmatch(pattern, lines[0]))


def _strict_require(job):
    for step in _steps(job):
        if (not _unconditional_step(step) or
                _step_field(step, "name") != "Require every test partition" or
                _step_env(job, step, "PARTITION_RESULTS") != "${{ toJSON(needs) }}"):
            continue
        blocks = extract_run_blocks("      - " + step)
        if len(blocks) != 1:
            continue
        body = textwrap.dedent("\n".join(blocks[0])).splitlines()
        if not body or not re.fullmatch(r"python3\s+-\s+<<['\"]?PY['\"]?", body[0].strip()):
            continue
        try:
            end = body.index("PY")
            tree = ast.parse("\n".join(body[1:end]))
        except (ValueError, SyntaxError):
            continue
        expected = ast.parse("""import json
import os
import sys
results = json.loads(os.environ['PARTITION_RESULTS'])
if not results or any(row.get('result') != 'success' for row in results.values()):
    print('::error::required test partition did not succeed; failing closed')
    sys.exit(1)
print('All required test partitions succeeded')
""")
        if ast.dump(tree, include_attributes=False) == ast.dump(expected, include_attributes=False):
            return True
    return False


def _ci_ok_strict(job):
    if _strict_require(job):
        return True
    result_vars = {
        'test-partition-plan': 'TEST_PARTITION_PLAN_RESULT',
        'rust-tests-partition': 'RUST_TEST_PARTITION_RESULT',
        'ignored-db-runtime-partition': 'RUNTIME_PARTITION_RESULT',
        'rust-tests': 'RUST_TESTS_RESULT',
        'ignored-db-runtime': 'IGNORED_DB_RUNTIME_RESULT',
    }
    for step in _steps(job):
        if not _unconditional_step(step):
            continue
        if (_step_env(job, step, 'JOB_INPUTS_RESULT') != '${{ needs.job-inputs.result }}' or
                _step_env(job, step, 'RUST_SELECTED') != '${{ needs.job-inputs.outputs.rust }}' or
                any(_step_env(job, step, var) != '${{ needs.' + lane + '.result }}'
                    for lane, var in result_vars.items())):
            continue
        blocks = extract_run_blocks('      - ' + step)
        if len(blocks) != 1:
            continue
        body = textwrap.dedent('\n'.join(blocks[0])).splitlines()
        if not body or not re.fullmatch(r"python3\s+-\s+<<['\"]?PY['\"]?", body[0].strip()):
            continue
        try:
            tree = ast.parse('\n'.join(body[1:body.index('PY')]))
        except (ValueError, SyntaxError):
            continue
        selected = 'def selected(name):\n    return os.environ.get(name, "") != "false"'
        fail = 'if failures:\n    print("ci-ok FAILED:")\n    for failure in failures:\n        print(f"  - {failure}")\n    sys.exit(1)'
        if not (_contains_stmt(tree.body, 'failures = []') and
                _contains_stmt(tree.body, selected) and _contains_stmt(tree.body, fail)):
            continue
        gate = next((n for n in tree.body if isinstance(n, ast.If) and
                     _same_ast(n.test, ast.parse('os.environ.get("JOB_INPUTS_RESULT") != "success"', mode='eval').body)), None)
        failure = '''failures.append("job-inputs did not succeed "
    f"(result={os.environ.get('JOB_INPUTS_RESULT')!r}); "
    "refusing to trust skips")'''
        if not gate or not gate.body or not _same_ast(gate.body[0], _stmt(failure)):
            continue
        gated = next((n.value for n in gate.orelse if isinstance(n, ast.Assign) and
                      len(n.targets) == 1 and isinstance(n.targets[0], ast.Name) and
                      n.targets[0].id == 'gated'), None)
        try:
            rows = ast.literal_eval(gated)
        except (ValueError, TypeError):
            continue
        if not isinstance(rows, list) or not all((lane, var, 'RUST_SELECTED') in rows
                                                  for lane, var in result_vars.items()):
            continue
        loop = '''for job, result_var, sel_var in gated:
    result = os.environ.get(result_var, "")
    if result == "success":
        continue
    if result == "skipped" and not selected(sel_var):
        continue
    failures.append(f"{job}: result={result!r} selected={selected(sel_var)}")'''
        if _contains_stmt(gate.orelse, loop):
            return True
    return False


def _partition_workflow(text):
    """Prove the connected planner, both workers, finals and ci-ok path."""
    jobs = _jobs(text)
    lanes = ("rust-tests", "ignored-db-runtime")
    planner = jobs.get("test-partition-plan", "")
    ci_ok = jobs.get("ci-ok", "")
    required = {"test-partition-plan", *(lane + "-partition" for lane in lanes), *lanes}
    if (not jobs.get("job-inputs") or not planner or not ci_ok or
            "job-inputs" not in _needs(planner) or
            "needs.job-inputs.outputs.rust != 'false'" != _field(planner, "if") or
            not required.issubset(_needs(ci_ok)) or "always()" not in _field(ci_ok, "if") or
            not _ci_ok_strict(ci_ok)):
        return False
    if not re.search(r"(?m)^      rust:\s*\S", jobs["job-inputs"]):
        return False
    for lane in lanes:
        if not re.search(r"(?m)^      " + re.escape(lane) +
                         r":\s*\$\{\{\s*steps\.plan\.outputs\." + re.escape(lane) + r"\s*\}\}\s*$", planner):
            return False
    plans = [s for s in _steps(planner) if _step_field(s, "id") == "plan"]
    if len(plans) != 1 or not _unconditional_step(plans[0]):
        return False
    blocks = extract_run_blocks("      - " + plans[0])
    plan = "\n".join(_logical_lines(blocks[0])) if len(blocks) == 1 else ""
    # The result of --list must be the matrix published under each loop lane.
    if not re.fullmatch(r'''\s*set\s+-euo\s+pipefail\s*
        for\s+lane\s+in\s+rust-tests\s+ignored-db-runtime;\s+do\s*
        matrix=\$\(python3\s+scripts/ci/run-test-partition\.py\s+--lane\s+["']\$lane["']\s+--list\)\s*
        printf\s+['"]%s=%s\\n['"]\s+["']\$lane["']\s+["']\$matrix["']\s+>>\s+["']\$GITHUB_OUTPUT["']\s*
        done\s*$''', plan, re.VERBOSE):
        return False
    command = r'python3\s+scripts/ci/run-test-partition\.py\s+--lane\s+["\']\$CI_TEST_LANE["\']\s+'
    for lane in lanes:
        worker = jobs.get(lane + "-partition", "")
        final = jobs.get(lane, "")
        if (not {"job-inputs", "test-partition-plan"}.issubset(_needs(worker)) or
                _field(worker, "if") != "needs.job-inputs.outputs.rust != 'false'" or
                _field(worker, "matrix", 6) !=
                "${{ fromJSON(needs.test-partition-plan.outputs." + lane + ") }}" or
                not {"job-inputs", lane + "-partition"}.issubset(_needs(final)) or
                _field(final, "if") != "always() && needs.job-inputs.outputs.rust != 'false'" or
                not _strict_require(final)):
            return False
        workers = [s for s in _steps(worker) if _step_field(s, "id") == "run_partition"]
        if not any(_step_env(worker, s, "CI_TEST_LANE") == lane and
                   _step_env(worker, s, "CI_TEST_PARTITION") == "${{ matrix.partition }}" and
                   _direct_run(s, command + r'--partition\s+["\']\$CI_TEST_PARTITION["\']')
                   for s in workers):
            return False
        if not any(_step_env(final, s, "CI_TEST_LANE") == lane and
                   _direct_run(s, command + r'--verify-receipts\s+test-partition-receipts')
                   for s in _steps(final)):
            return False
    return True


def _same_ast(left, right):
    return ast.dump(left, include_attributes=False) == ast.dump(right, include_attributes=False)


def _stmt(source):
    return ast.parse(source).body[0]


def _contains_stmt(body, source):
    expected = _stmt(source)
    return any(_same_ast(node, expected) for node in body)


def _has_unrecognized_exit(body, allowed=()):
    """Fail closed on exits nested in any unrecognized statement.

    The few legitimate selection and validation branches are accepted only as
    complete AST matches; merely nesting an exit inside a conditional, loop,
    or try block cannot make it invisible to the routing proof.
    """
    allowed_nodes = [_stmt(source) for source in allowed]
    return any(
        any(isinstance(child, (ast.Return, ast.Raise)) for child in ast.walk(node))
        for node in body
        if not any(_same_ast(node, accepted) for accepted in allowed_nodes)
    )


def _runner_routes(root):
    """Recognize the runner's narrow executable AST interface, without importing it.

    The proof follows main -> lane dispatch -> command selection -> cargo
    execution. Discovery and owners are read from live statements, never from
    comments or free-standing string literals. Unknown rewrites fail closed.
    """
    try:
        tree = ast.parse((root / "scripts/ci/run-test-partition.py").read_text())
    except (OSError, SyntaxError, UnicodeError):
        return False, False, False, set()
    functions = {n.name: n for n in tree.body if isinstance(n, ast.FunctionDef)}
    if any(sum(isinstance(n, ast.FunctionDef) and n.name == name for n in tree.body) != 1
           for name in ("main", "execute", "rust_commands", "integration_commands", "runtime_commands", "command", "verify_receipts")):
        return False, False, False, set()
    main = functions["main"].body
    execute = functions["execute"].body
    rust = functions["rust_commands"].body
    integration = functions["integration_commands"].body
    runtime = functions["runtime_commands"].body
    execution_loop = 'for index, (args, database, integration, timeout, budget) in enumerate(commands):\n    print("Running:", shlex.join(["cargo"] + args), flush=True)\n    start = time.monotonic()\n    subprocess.run(["cargo"] + args, env=environment(database, integration), check=True, timeout=remaining_timeout(index, args, timeout, deadlines))\n    if budget:\n        property_seconds += time.monotonic() - start'
    if not (tree.body and isinstance(tree.body[-1], ast.If) and
            _same_ast(tree.body[-1].test, ast.parse('__name__ == "__main__"', mode='eval').body) and
            len(tree.body[-1].body) == 1 and isinstance(tree.body[-1].body[0], ast.Try) and
            tree.body[-1].body[0].body and
            _same_ast(tree.body[-1].body[0].body[0], _stmt('main()')) and
            _contains_stmt(main, 'parser.add_argument("--lane", required=True, action="append", choices=PARTITIONS)') and
            _contains_stmt(main, 'selection = parser.add_mutually_exclusive_group(required=True)') and
            _contains_stmt(main, 'selection.add_argument("--list", action="append_const", const=True)') and
            _contains_stmt(main, 'selection.add_argument("--partition", action="append")') and
            _contains_stmt(main, 'selection.add_argument("--verify-receipts", action="append")') and
            _contains_stmt(main, 'args = parser.parse_args()') and
            _contains_stmt(main, 'lane = args.lane[0]') and
            _contains_stmt(main, 'if args.list:\n    print(json.dumps({"include": [{"partition": p} for p in PARTITIONS[lane]]}, separators=(",", ":")))\n    return') and
            _contains_stmt(main, 'if len(args.partition) != 1 or args.partition[0] not in PARTITIONS[lane]:\n    parser.error("partition must be specified once and belong to the selected lane")') and
            _contains_stmt(main, 'partition = args.partition[0]') and
            _contains_stmt(main, 'if args.verify_receipts is not None:\n    if len(args.verify_receipts) != 1 or not args.verify_receipts[0]:\n        parser.error("receipt directory must be specified exactly once")\n    verify_receipts(lane, args.verify_receipts[0])\n    return') and
            _contains_stmt(main, 'commands = rust_commands(partition) if lane == "rust-tests" else runtime_commands(partition)') and
            _contains_stmt(main, 'seconds = execute(commands)') and
            _contains_stmt(main, 'write_receipt(lane, partition, seconds)') and
            _contains_stmt(execute, execution_loop) and
            _contains_stmt(functions["command"].body, 'return (args, database, integration, timeout, budget)')):
        return False, False, False, set()
    # The receipt check must validate exactly the expected partition files.
    verify = functions['verify_receipts'].body
    if not (_contains_stmt(verify, 'expected = {f"{lane}-{partition}.json": partition for partition in PARTITIONS[lane]}') and
            _contains_stmt(verify, 'entries = list(directory.iterdir())') and
            _contains_stmt(verify, 'if {p.name for p in entries} != set(expected) or any(not p.is_file() or p.is_symlink() for p in entries):\n    raise ValueError("receipt directory must contain exactly one file for each lane partition")')):
        return False, False, False, set()
    # Exits before dispatch or Cargo execution must be recognized validation or
    # selection branches, even when nested under another control-flow statement.
    main_end = next(i for i, n in enumerate(main) if _same_ast(n, _stmt('seconds = execute(commands)')))
    execute_end = next(i for i, n in enumerate(execute) if _same_ast(n, _stmt(execution_loop)))
    if (_has_unrecognized_exit(main[:main_end], (
            'if args.list:\n    print(json.dumps({"include": [{"partition": p} for p in PARTITIONS[lane]]}, separators=(",", ":")))\n    return',
            'if args.verify_receipts is not None:\n    if len(args.verify_receipts) != 1 or not args.verify_receipts[0]:\n        parser.error("receipt directory must be specified exactly once")\n    verify_receipts(lane, args.verify_receipts[0])\n    return')) or
            _has_unrecognized_exit(execute[:execute_end], (
                'if not has_tests(commands, deadlines):\n    raise ValueError("partition selected no tests")',))):
        return False, False, False, set()
    integration_dispatch = _contains_stmt(rust, 'if partition.startswith("integration-"):\n    return integration_commands(int(partition[-1]))')
    discovery = (integration_dispatch and integration and
        not _has_unrecognized_exit(integration[:-1], (
            'if core is None or not members.issubset(nodes):\n    raise ValueError("Cargo metadata is missing workspace dependency resolution")',)) and
        isinstance(integration[0], ast.Assign) and len(integration[0].targets) == 1 and
        isinstance(integration[0].targets[0], ast.Name) and integration[0].targets[0].id == 'proc' and
        isinstance(integration[0].value, ast.Call) and
        _same_ast(integration[0].value.func, ast.parse('subprocess.run', mode='eval').body) and
        isinstance(integration[0].value.args[0], ast.List) and
        [getattr(x, 'value', None) for x in integration[0].value.args[0].elts[:2]] == ['cargo', 'metadata'] and
        _contains_stmt(integration, 'graph = json.loads(proc.stdout)') and
        _contains_stmt(integration, 'members = set(graph["workspace_members"])') and
        _contains_stmt(integration, 'packages = [p for p in graph["packages"] if p["id"] in members]') and
        _contains_stmt(integration, 'by_package = {}') and
        _contains_stmt(integration, 'result = []') and
        _same_ast(integration[-1], _stmt('return result')) and
        _contains_stmt(integration, 'for package in packages:\n    for target in package["targets"]:\n        if "test" not in target["kind"] or not target.get("test", True):\n            continue\n        required = target.get("required-features", target.get("required_features", []))\n        if not set(required).issubset(nodes[package["id"]].get("features", [])):\n            continue\n        key = package["name"] + "/" + target["name"]\n        owner = int.from_bytes(hashlib.sha256(key.encode()).digest()[:8], "big") % 3\n        if owner == index:\n            by_package.setdefault(package["name"], []).append(target["name"])'))
    # Verify the selected targets become Cargo --test selectors and executed commands.
    package_loop = next((n for n in integration if isinstance(n, ast.For) and
                         ast.unparse(n.target) == '(name, targets)' and
                         _same_ast(n.iter, ast.parse('sorted(by_package.items())', mode='eval').body)), None)
    discovery = bool(discovery and package_loop and
        _contains_stmt(package_loop.body, 'selectors = [word for target in sorted(targets) for word in ("--test", target)]') and
        _contains_stmt(package_loop.body, 'base = ["test", "-p", name] + selectors + feature_args + ["--locked"]') and
        _contains_stmt(package_loop.body, 'result.append(command(base + ["--"] + skips, BOOTSTRAP, integration=True))'))
    def rust_branch(partition):
        return next((n for n in rust if isinstance(n, ast.If) and
                     _same_ast(n.test, ast.parse('partition == ' + repr(partition), mode='eval').body)), None)
    def returned_calls(value):
        # Only calls directly in returned lists (possibly concatenated) are
        # executed. Do not count calls in lambdas, unused literals or filters.
        if isinstance(value, ast.BinOp) and isinstance(value.op, ast.Add):
            return returned_calls(value.left) + returned_calls(value.right)
        if isinstance(value, ast.List):
            return [n for n in value.elts if isinstance(n, ast.Call) and
                    isinstance(n.func, ast.Name) and n.func.id == 'command' and n.args]
        return []

    def has_command_list(branch, values):
        if branch is None or not branch.body or not isinstance(branch.body[0], ast.Return):
            return False
        for call in returned_calls(branch.body[0].value):
            node = call.args[0]
            while isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
                node = node.left
            if isinstance(node, ast.List) and [getattr(e, 'value', None) for e in node.elts] == values:
                return True
        return False
    unit = has_command_list(rust_branch('unit-bin'), ['test', '--workspace', '--lib', '--bins'])
    doc = has_command_list(rust_branch('replays'), ['test', '--workspace', '--doc'])
    owners = set()
    partitions = next((n.value for n in tree.body if isinstance(n, ast.Assign) and
                       len(n.targets) == 1 and isinstance(n.targets[0], ast.Name) and
                       n.targets[0].id == 'PARTITIONS'), None)
    try:
        partition_map = ast.literal_eval(partitions)
    except (ValueError, TypeError):
        return False, False, False, set()
    if (not isinstance(partition_map, dict) or
            not {'unit-bin', 'integration-0', 'integration-1', 'integration-2', 'replays'}
                .issubset(partition_map.get('rust-tests', ()))):
        discovery = unit = doc = False

    def literal_prefix(node):
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
            return literal_prefix(node.left) + literal_prefix(node.right)
        if isinstance(node, ast.Name) and node.id == 'bot':
            return ['test', '-p', 'two-bot', '--locked']
        try:
            value = ast.literal_eval(node)
        except (ValueError, TypeError):
            return []
        return value if isinstance(value, list) else []

    if _contains_stmt(runtime, 'bot = ["test", "-p", "two-bot", "--locked"]'):
        for branch in runtime:
            if (not isinstance(branch, ast.If) or not isinstance(branch.test, ast.Compare) or
                    not isinstance(branch.test.left, ast.Name) or branch.test.left.id != 'partition' or
                    len(branch.test.ops) != 1 or not isinstance(branch.test.ops[0], ast.Eq) or
                    len(branch.test.comparators) != 1 or
                    not isinstance(branch.test.comparators[0], ast.Constant) or
                    branch.test.comparators[0].value not in partition_map.get('ignored-db-runtime', ())):
                continue
            if branch.body and isinstance(branch.body[0], ast.Return):
                for node in returned_calls(branch.body[0].value):
                    args = literal_prefix(node.args[0])
                    if len(args) >= 3 and args[:2] == ['test', '-p'] and '--test' in args:
                        i = args.index('--test')
                        if i + 1 < len(args) and isinstance(args[i + 1], str):
                            owners.add((args[2], args[i + 1]))
            # This branch builds the cutover list from literal (target, DB,
            # ignored) tuples and then returns both comprehensions.
            if (branch.test.comparators[0].value == 'cutover' and
                    _contains_stmt(branch.body, 'base = ["test", "-p", "two-bot-cutover"]') and
                    _contains_stmt(branch.body, 'result = [command(base + ["--test", name, "--locked"] + (["--", "--ignored"] if ignored else []), db) for name, db, ignored in items]') and
                    _contains_stmt(branch.body, 'result += [command(base + ["--locked", "--test", name, "--", "--ignored"]) for name in ("invite_store", "member_list_catchup")]') and
                    _contains_stmt(branch.body, 'return result + [command(base + ["--locked", "--lib", "--", "--ignored"])]')):
                items = next((n.value for n in branch.body if isinstance(n, ast.Assign) and
                              len(n.targets) == 1 and isinstance(n.targets[0], ast.Name) and
                              n.targets[0].id == 'items' and isinstance(n.value, ast.List)), None)
                if items:
                    for item in items.elts:
                        if (isinstance(item, ast.Tuple) and len(item.elts) == 3 and
                                isinstance(item.elts[0], ast.Constant) and
                                isinstance(item.elts[0].value, str) and
                                isinstance(item.elts[2], ast.Constant) and item.elts[2].value is True):
                            owners.add(('two-bot-cutover', item.elts[0].value))
                    owners.update({('two-bot-cutover', 'invite_store'),
                                   ('two-bot-cutover', 'member_list_catchup')})
    return discovery, unit, doc, owners


def check(root=ROOT):
    errors = []
    try:
        check_yml = (root / ".github/workflows/check.yml").read_text()
        nightly_yml = (root / ".github/workflows/nightly.yml").read_text()
    except OSError as exc:
        return [f"workflows unreadable: {exc}"]
    routed, executable_texts = routed_targets(check_yml, nightly_yml)
    # Lane-presence markers must be executable `run:` text, not a mention
    # elsewhere in the workflow YAML (comment, step/job name, env value,
    # echo/printf print). Reuse the same executable texts the routing
    # analysis already computes so a stripped mention cannot keep the gate
    # green while the lane no longer runs.
    check_exec = executable_texts[0]
    nightly_exec = executable_texts[1] if len(executable_texts) > 1 else ""
    blanket_routed = bool(_BLANKET_RE.search(check_exec))
    discovery = unit = doc = False
    owners = set()
    if _partition_workflow(check_yml):
        discovery, unit, doc, owners = _runner_routes(root)

    for path in sorted(root.glob("crates/*/tests/*.rs")):
        name = path.stem
        has_ignored = bool(re.search(r"#\[ignore", path.read_text(errors="replace")))
        if name in EXEMPT:
            if not EXEMPT[name].strip():
                errors.append(f"{name}: exemption reason is empty")
            continue
        if name in routed:
            continue
        if not has_ignored and (blanket_routed or discovery):
            continue
        if has_ignored and owners:
            try:
                package = tomllib.loads((path.parent.parent / "Cargo.toml").read_text())["package"]["name"]
            except (OSError, tomllib.TOMLDecodeError, KeyError, TypeError):
                package = None
            if (package, name) in owners:
                continue
        errors.append(f"orphaned test target: {name} "
                      f"({'has #[ignore]d tests' if has_ignored else 'not referenced'}; "
                      "route it to a lane or exempt it with a reason)")

    try:
        manifest = tomllib.loads((root / "fuzz/Cargo.toml").read_text())
        bins = [b["name"] for b in manifest.get("bin", []) if "name" in b]
    except (OSError, tomllib.TOMLDecodeError) as exc:
        return errors + [f"fuzz manifest unreadable: {exc}"]
    for name in bins:
        if not (root / "fuzz" / "fuzz_targets" / f"{name}.rs").is_file():
            errors.append(f"fuzz target {name} has no fuzz/fuzz_targets/{name}.rs")
    if FUZZ_BUILD not in check_exec:
        errors.append("fuzz-compile job does not build all fuzz targets "
                      f"(expected {FUZZ_BUILD!r})")

    for marker, label in ((WORKSPACE_LIB_BINS, "workspace lib/bins unit run"),
                          (WORKSPACE_DOC, "workspace doc-test run"),
                          ("--all-targets", "all-targets build (examples)")):
        runner_marker = (marker == WORKSPACE_LIB_BINS and unit) or (marker == WORKSPACE_DOC and doc)
        if marker not in check_exec and marker not in nightly_exec and not runner_marker:
            errors.append(f"missing {label} ({marker!r} not found in check/nightly)")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    errors = check(args.root)
    for error in errors:
        print(f"test inventory: {error}", file=sys.stderr)
    if not errors:
        print("test inventory: every target is routed to a CI job")
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
