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
2. Integration targets without ``#[ignore]`` are blanket-covered by the
   workspace ``--test '*'`` integration run (asserted present in an
   executable ``run:`` command in ``check.yml``).
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
for-loop list in rule 1), follow generated commands, or count a bare
test-name filter without ``--test`` as routing an ignored target. A target
routed only through such a shape should be switched to an explicit
``--test <name>`` command, or exempted with a reason.
"""

import argparse
import re
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


def routed_targets(*workflow_texts):
    """Return (executably routed target names, executable texts per workflow)."""
    routed = set()
    executable_texts = []
    for text in workflow_texts:
        blocks = [executable_block_text(body) for body in extract_run_blocks(text)]
        executable_texts.append("\n".join(blocks))
        for block in blocks:
            for segment in block.splitlines():
                for arg in _TEST_ARG_RE.finditer(segment):
                    if _INVOCATION_RE.search(segment[:arg.start()]):
                        routed.add(arg.group(1))
            for loop in _LOOP_RE.finditer(block):
                var = re.escape(loop.group("var"))
                runs_loop_var = any(
                    (arg := re.search(
                        r"--test(?:\s+|=)[\"']?\$(?:\{%s\}|%s\b)" % (var, var),
                        line)) and _INVOCATION_RE.search(line[:arg.start()])
                    for line in loop.group("body").splitlines())
                if runs_loop_var:
                    routed.update(
                        token.strip("\"'")
                        for token in loop.group("list").split())
    return routed, executable_texts


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

    for path in sorted(root.glob("crates/*/tests/*.rs")):
        name = path.stem
        has_ignored = bool(re.search(r"#\[ignore", path.read_text(errors="replace")))
        if name in EXEMPT:
            if not EXEMPT[name].strip():
                errors.append(f"{name}: exemption reason is empty")
            continue
        if name in routed:
            continue
        if not has_ignored and blanket_routed:
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
        if marker not in check_exec and marker not in nightly_exec:
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
