#!/usr/bin/env python3
"""Offline parity/checklist coverage and rendered-document guard (stdlib only).

Supported automated verification runs Cargo through the controller cache
wrapper only: `python3 scripts/cargo_cache.py run -- test -p <package>
[--lib | --test <target> | --tests] [filter] [-- <libtest>]`. The supported
option vocabulary is bounded: Cargo side `-p`/`--package`, `--test`,
`--lib`, `--tests`, `--all-targets`, `--features`; libtest side (after `--`)
`--exact`, `--ignored`, `--include-ignored`, `--skip <pattern>`
(repeatable), `--test-threads <n>`. Cargo `--no-run` and libtest `--list`
never execute tests and are refused as automated coverage, as are unknown
flags, missing or invalid option values, and known options in the wrong
position. Cargo accepts at most one positional filter before `--`; pass
additional filters after `--` where libtest accepts several. Effective
selection is the union of positive filters (exact when `--exact`, else
substring) under ignore-mode filtering (default excludes `#[ignore]`,
`--ignored` selects only ignored, `--include-ignored` selects all matched)
minus `--skip` patterns, read offline from source and never run; it must be
nonempty. Static executable selection is not an execution verdict: attach
actual run evidence separately.

Offline regression: `PYTHONDONTWRITEBYTECODE=1 python3 scripts/check_soak_checklist.py`
plus `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p
'test_soak_checklist.py' -v` and `-p 'test_soak_checklist_noexec_acceptance.py' -v`.
"""

import argparse
from collections import Counter
from functools import cache
import json
from pathlib import Path
import re
import shlex
import tomllib

ROOT = Path(__file__).resolve().parents[1]
CONFIG_ROW = (7, ("Config / env catalogue",))
PARITY_SECTIONS = tuple(range(1, 9))
# §12 additions and §13 non-replay ledger obligations (parity §11 B4 gate).
DELTA_SECTIONS = (12, 13)
SECTIONS = PARITY_SECTIONS + DELTA_SECTIONS
DISPOSITION_HEADERS = ("Disposition", "Next boundary / disposition")
LEDGER_HEADER = ("Legacy commit", "Area", "Change", "Status", "Disposition")
LEDGER_STATUSES = ("ported", "carded", "gap", "dropped")
CARD = re.compile(r"\bTOG-\d+\b")
SCRIPT_REF = re.compile(r"(?<![\w/.-])((?:\.{1,2}/)*(?:\.github/)?scripts/[\w./-]+\.\w+)")
# Automated rows run Cargo through the controller cache wrapper only.
WRAPPER = ("python3", "scripts/cargo_cache.py", "run", "--")
CARGO_VALUE_OPTIONS = ("-p", "--package", "--test", "--features")
CARGO_FLAG_OPTIONS = ("--lib", "--tests", "--all-targets")
LIBTEST_VALUE_OPTIONS = ("--skip", "--test-threads")
LIBTEST_FLAG_OPTIONS = ("--exact", "--ignored", "--include-ignored")
# Cargo --no-run and libtest --list never execute tests; both are refused
# as automated coverage wherever they appear.
NOEXEC_OPTIONS = ("--no-run", "--list")
UNSUPPORTED_TARGETS = ("--bin", "--bins", "--example", "--examples", "--bench",
                       "--benches", "--doc")
CARGO_KNOWN = CARGO_VALUE_OPTIONS + CARGO_FLAG_OPTIONS + UNSUPPORTED_TARGETS
LIBTEST_KNOWN = LIBTEST_VALUE_OPTIONS + LIBTEST_FLAG_OPTIONS
RUST_TOKEN = re.compile(r"""
    (?P<space>\s+|//[^\n]*)
  | (?P<block>/\*)
  | (?P<raw>[bc]?r(?P<hashes>\#*)")
  | (?P<string>[bc]?"(?:\\.|[^\\"])*")
  | (?P<char>b?'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^\\'\n])')
  | (?P<lifetime>'(?:r\#)?[^\W\d]\w*)
  | (?P<ident>(?:r\#)?[^\W\d]\w*)
  | (?P<number>\d\w*)
  | (?P<punct>::|.)
""", re.X | re.S)


def split_cells(text):
    """Split a table line with optional leading/trailing border pipes."""
    body = text
    if body.startswith("|"):
        body = body[1:]
    if body.endswith("|") and not body.endswith("\\|"):
        body = body[:-1]
    # Escaped pipes are cell content, not table boundaries.
    return tuple(c.strip().replace(r"\|", "|") for c in
                 re.split(r"(?<!\\)\|", body))


def mapped_owner(mapping):
    """Slice/issue owner that keeps a DROP-prefixed row in scope."""
    return re.search(r"\b(?:S\d+|B\d+|NEW-\d+|TOG-\d+)\b", mapping)


def looks_mapped(cell):
    """True when a last-cell value claims parity mapping of any kind."""
    mapping = cell.replace("*", "").strip()
    return bool(re.match(r"^DROP\b", mapping) or mapped_owner(mapping))


def table_rows(markdown, sections, last_headers):
    """Body rows of the tables in numbered `sections`, failing closed.

    Returns ``(rows, seen)``: rows are ``(section, headers, cells)`` and seen
    holds every requested section heading present in the document.
    """
    section = None
    headers = None
    rows = []
    seen = set()
    for line in markdown.splitlines():
        heading = re.match(r"^## (\d+)\. ", line)
        if re.match(r"^##\s", line):
            section = int(heading[1]) if heading else None
            headers = None
            if section in sections:
                seen.add(section)
            continue
        if section not in sections:
            continue
        stripped = line.strip()
        if not stripped or "|" not in stripped:
            headers = None
            continue
        cells = split_cells(stripped)
        if all(re.fullmatch(r":?-+:?", c) for c in cells):
            continue
        if headers is None:
            if cells and cells[-1] in last_headers:
                # A bordered or borderless header opens table scope.
                headers = cells
            elif not stripped.startswith("|") and looks_mapped(cells[-1]):
                # Fail closed: a borderless mapped row separated from its
                # table by a blank line must not silently skip coverage
                # either. Border the row or reword the prose.
                raise ValueError(
                    f"§{section}: unsupported borderless table row: {stripped[:60]}")
            elif stripped.startswith("|"):
                raise ValueError(
                    f"§{section}: parity table must end in {' or '.join(last_headers)}")
            # Borderless prose carrying pipes is ordinary prose, not a table.
            continue
        if not stripped.startswith("|"):
            # Fail closed: a GFM body row without its leading border must
            # not silently skip coverage while a table is active.
            raise ValueError(
                f"§{section}: table row without leading border: {stripped[:60]}")
        if len(cells) != len(headers):
            raise ValueError(f"§{section}: malformed table row: {cells}")
        rows.append((section, headers, cells))
    return rows, seen


def parity_rows(markdown):
    """Read Map tables in §§1–8; exclude only wholly DROP-mapped rows."""
    rows = []
    table, seen_sections = table_rows(markdown, PARITY_SECTIONS, ("Map",))
    for section, _, cells in table:
        mapping = cells[-1].replace("*", "").strip()
        if not mapping:
            raise ValueError(f"§{section}: unmapped row: {cells}")
        # A DROP prefix can still carry mapped work, including replacement
        # owners in parentheses. Semicolons also occur within drop reasons.
        if mapped_owner(mapping) or not re.match(r"^DROP\b", mapping):
            rows.append((section, cells[:-1]))
    if seen_sections != set(PARITY_SECTIONS):
        raise ValueError("Expected all parity sections 1–8")
    # §7 is a prose catalogue, not a table. Still require explicit coverage.
    config = re.search(r"^## 7\. .*?\n(.*?)(?=^## 8\.)", markdown, re.M | re.S)
    if not config or not all(word in config[1] for word in ("env_only", "cold", "hot")):
        raise ValueError("§7 must describe env_only/cold/hot config classes")
    rows.append(CONFIG_ROW)
    counts = Counter(section for section, _ in rows)
    if set(counts) != set(PARITY_SECTIONS):
        raise ValueError("Every parity section must have mapped coverage")
    if len(rows) != len(set(rows)):
        raise ValueError("Duplicate parity source rows")
    return rows


def unlink(cell):
    """Render a source cell as a heading: keep link text, drop targets."""
    return re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", cell)


def owner_cards(cell):
    """TOG cards a disposition cell names, in first-mention order."""
    return tuple(dict.fromkeys(CARD.findall(cell)))


def delta_rows(markdown):
    """Map §12 rows and §13 non-dropped ledger rows to their owner cards.

    Keys copy every source cell except the disposition, like ``parity_rows``.
    §12 keeps the existing DROP rule; §13 excludes ``dropped`` ledger rows
    (reasoned drops, including all history-rewrite replays), whose features
    stay mapped in §§1–8. Every retained row must name an owner card, except
    ``ported`` ledger rows, which cite source evidence instead.
    """
    rows = {}
    table, seen = table_rows(markdown, DELTA_SECTIONS, DISPOSITION_HEADERS)
    if seen != set(DELTA_SECTIONS):
        raise ValueError("Expected parity sections 12 and 13")
    for section, headers, cells in table:
        disposition = cells[-1].replace("*", "").strip()
        if not disposition:
            raise ValueError(f"§{section}: row without disposition: {cells}")
        owners = owner_cards(disposition)
        if section == 13:
            if headers != LEDGER_HEADER:
                raise ValueError(f"§13: ledger columns must be {' | '.join(LEDGER_HEADER)}")
            status = cells[3]
            if status not in LEDGER_STATUSES:
                raise ValueError(f"§13: unknown ledger status {status!r}: {cells[0]}")
            if status == "dropped":
                continue
            if status != "ported" and not owners:
                raise ValueError(f"§13: {status} row needs an owner card: {cells[0]}")
        elif re.match(r"^DROP\b", disposition) and not mapped_owner(disposition):
            continue
        elif not owners:
            raise ValueError(f"§12: row needs an owner card: {cells[0][:60]}")
        key = (section, cells[:-1])
        if key in rows:
            raise ValueError("Duplicate parity source rows")
        rows[key] = owners
    counts = Counter(section for section, _ in rows)
    if set(counts) != set(DELTA_SECTIONS):
        raise ValueError("§12 and §13 must each have retained coverage")
    return rows


def rust_tokens(source):
    """Identifier, string and punctuation tokens; comments and chars dropped."""
    tokens = []
    pos = 0
    while pos < len(source):
        match = RUST_TOKEN.match(source, pos)
        kind = match.lastgroup if match.lastgroup != "hashes" else "raw"
        pos = match.end()
        if kind == "block":
            depth = 1
            while depth:
                close = source.find("*/", pos)
                if close < 0:
                    raise ValueError("unterminated block comment")
                opened = source.find("/*", pos, close)
                depth, pos = (depth + 1, opened + 2) if opened >= 0 else (depth - 1, close + 2)
        elif kind == "raw":
            end = source.find('"' + match["hashes"], pos)
            if end < 0:
                raise ValueError("unterminated raw string")
            tokens.append(("string", source[pos:end]))
            pos = end + 1 + len(match["hashes"])
        elif kind == "string":
            tokens.append(("string", re.sub(r'^[bc]?"|"$', "", match[0])))
        elif kind in ("ident", "punct"):
            tokens.append((kind, match[0]))
    return tokens


def is_test_attribute(tokens):
    """`#[test]`, `#[tokio::test(...)]` and similar `...::test` attributes."""
    end = 0
    while (end + 2 < len(tokens) and tokens[end][0] == "ident"
           and tokens[end + 1] == ("punct", "::")):
        end += 2
    return (end < len(tokens) and tokens[end] == ("ident", "test")
            and tokens[end + 1:end + 2] in ([], [("punct", "(")]))


def is_ignore_attribute(tokens):
    """`#[ignore]` and `#[ignore = "..."]` attributes."""
    return bool(tokens) and tokens[0] == ("ident", "ignore")


def _libtest_entries(path, mod_rs=True, prefix=(), seen=None):
    """`(name, ignored)` pairs of test fns reachable from a target root.

    Follows `mod name;` (including `#[path]`) and inline `mod name { }` like
    rustc, ignoring cfg: a renamed or removed test leaves no name behind.
    Frames hold a module name, "" for a macro body or None for other blocks.
    Ignored identity comes from `#[ignore]` on the test fn; inline, external
    file and integration targets all preserve it.
    """
    seen = set() if seen is None else seen
    if path.resolve() in seen:
        return []
    seen.add(path.resolve())
    tokens = rust_tokens(path.read_text())
    entries = []
    frames = []
    attributes = []
    i = 0
    while i < len(tokens):
        token = tokens[i]
        following = tokens[i + 1:i + 3]
        start = i + 2 if tokens[i + 1:i + 2] == [("punct", "!")] else i + 1
        if token == ("punct", "#") and tokens[start:start + 1] == [("punct", "[")]:
            depth = 0
            for end in range(start, len(tokens)):
                depth += {("punct", "["): 1, ("punct", "]"): -1}.get(tokens[end], 0)
                if not depth:
                    break
            if start == i + 1:
                attributes.append(tokens[start + 1:end])
            i = end + 1
            continue
        inline = [frame for frame in frames if frame]
        if token in (("punct", "{"), ("punct", "}"), ("punct", ";")):
            if token[1] == "{":
                # `proptest! { #[test] fn ... }` emits its tests in place;
                # any other block (fn, impl, ...) hides nested items.
                macro = i >= 2 and tokens[i - 1] == ("punct", "!") and tokens[i - 2][0] == "ident"
                frames.append("" if macro else None)
            elif token[1] == "}":
                if not frames:
                    raise ValueError(f"unbalanced braces in {path}")
                frames.pop()
            attributes = []
        elif token == ("ident", "mod") and len(following) == 2 and following[0][0] == "ident":
            name = following[0][1]
            if following[1] == ("punct", "{"):
                frames.append(name)
                attributes = []
                i += 3
                continue
            if following[1] == ("punct", ";"):
                directory = path.parent if mod_rs else path.parent / path.stem
                explicit = [a[2][1] for a in attributes
                            if len(a) == 3 and a[:2] == [("ident", "path"), ("punct", "=")]]
                if explicit:
                    # `#[path]` files own their directory, like `mod.rs`.
                    base = directory.joinpath(*inline) if inline else path.parent
                    candidates = ((base / explicit[0], True),)
                else:
                    base = directory.joinpath(*inline)
                    candidates = ((base / f"{name}.rs", False), (base / name / "mod.rs", True))
                for child, child_mod_rs in candidates:
                    if child.is_file():
                        entries += _libtest_entries(child, child_mod_rs,
                                                    (*prefix, *inline, name), seen)
                        break
        elif (token == ("ident", "fn") and following and following[0][0] == "ident"
              and None not in frames and any(map(is_test_attribute, attributes))):
            name = "::".join((*prefix, *inline, following[0][1]))
            entries.append((name, any(map(is_ignore_attribute, attributes))))
        i += 1
    return entries


def libtest_names(path, mod_rs=True, prefix=(), seen=None):
    """Libtest names (`module::fn`) of test fns reachable from a target root.

    Follows `mod name;` (including `#[path]`) and inline `mod name { }` like
    rustc, ignoring cfg: a renamed or removed test leaves no name behind.
    Frames hold a module name, "" for a macro body or None for other blocks.
    """
    return [name for name, _ in _libtest_entries(path, mod_rs, prefix, seen)]


@cache
def target_tests(root):
    """Test names of one target root; sources are fixed for a process."""
    return frozenset(libtest_names(root))


@cache
def target_ignored_tests(root):
    """Ignored test names of one target root; sources are fixed for a process."""
    return frozenset(name for name, ignored in _libtest_entries(root) if ignored)


def workspace_packages(root):
    """Workspace package name -> crate directory, from the Cargo manifests."""
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    directories = [root] + [root / member for member in workspace["members"]]
    manifests = {d: tomllib.loads((d / "Cargo.toml").read_text()) for d in directories}
    return {m["package"]["name"]: d for d, m in manifests.items() if "package" in m}


def crate_targets(directory):
    """Testable target roots of one crate, by kind and target name."""
    manifest = tomllib.loads((directory / "Cargo.toml").read_text())
    targets = {"lib": {}, "bin": {}, "test": {}}
    if (directory / "src/lib.rs").is_file():
        targets["lib"][manifest["package"]["name"]] = directory / "src/lib.rs"
    roots = [directory / "src/main.rs"] + sorted(directory.glob("src/bin/*.rs"))
    roots += [directory / b["path"] for b in manifest.get("bin", []) if "path" in b]
    targets["bin"] = {root.stem: root for root in roots if root.is_file()}
    for root in sorted(directory.glob("tests/*.rs")) + sorted(directory.glob("tests/*/main.rs")):
        targets["test"][root.stem if root.name != "main.rs" else root.parent.name] = root
    for test in manifest.get("test", []):
        targets["test"][test["name"]] = directory / test.get("path", f"tests/{test['name']}.rs")
    return targets


def cargo_commands(verification):
    """Cargo argument lists of each wrapper invocation in a shell command."""
    lexer = shlex.shlex(verification, posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    commands = [[]]
    for word in lexer:
        if set(word) <= set("();<>|&"):
            commands.append([])
        else:
            commands[-1].append(word)
    found = []
    for words in commands:
        if tuple(words[:4]) == WRAPPER:
            found.append(words[4:])
        elif any(Path(word).name in ("cargo", "cargo_cache.py") for word in words):
            raise ValueError(f"run Cargo as `{' '.join(WRAPPER)} test ...`: {' '.join(words)}")
    return found


def check_cargo_test(args, root, packages):
    """Fail unless `cargo test` args name a real package, target and tests."""
    if args[:1] != ["test"]:
        raise ValueError(f"automated verification must be `cargo test`: {' '.join(args)}")
    names, selectors, filters, skips = [], [], [], []
    exact, ignored_only, include_ignored, libtest = False, False, False, False
    test_threads_seen = False
    pre_filters = 0
    words = iter(args[1:])
    for word in words:
        option, has_value, value = word.partition("=")
        if libtest:
            if option in NOEXEC_OPTIONS:
                raise ValueError(
                    f"NOEXEC: refusing non-executing mode {option!r} as automated coverage; "
                    f"next: remove {option} so the command actually runs tests")
            if option in LIBTEST_VALUE_OPTIONS:
                if option == "--test-threads" and test_threads_seen:
                    raise ValueError(
                        f"UNSUPPORTED: libtest option '--test-threads' is single-use "
                        f"but was repeated; next: pass --test-threads at most once")
                if has_value:
                    if not value:
                        raise ValueError(
                            f"INVALID_VALUE: option {option!r} got an empty value after `=`; "
                            f"next: give a nonempty value or drop `=value`")
                else:
                    value = next(words, None)
                    if value is None:
                        raise ValueError(
                            f"MISSING_VALUE: option {option!r} needs a value but none followed; "
                            f"next: add a value, e.g. `{option} <value>` or `{option}=<value>`")
                if option == "--test-threads":
                    # ASCII only: str.isdigit is Unicode-aware, but libtest
                    # parses an ASCII integer (and int() raises on some
                    # digit-like codepoints instead of returning a count).
                    if not value.isascii() or not value.isdigit() or int(value) <= 0:
                        raise ValueError(
                            f"INVALID_VALUE: option '--test-threads' needs a positive integer "
                            f"but got {value!r}; next: pass a positive integer like "
                            f"`--test-threads 2`")
                    test_threads_seen = True
                else:
                    skips.append(value)
            elif option in LIBTEST_FLAG_OPTIONS:
                if has_value:
                    raise ValueError(
                        f"INVALID_VALUE: flag {option!r} takes no value but got {value!r}; "
                        f"next: drop `={value}` and pass {option} alone")
                if option == "--exact":
                    if exact:
                        raise ValueError(
                            f"UNSUPPORTED: libtest option '--exact' is single-use "
                            f"but was repeated; next: pass --exact at most once")
                    exact = True
                elif option == "--ignored":
                    if ignored_only:
                        raise ValueError(
                            f"UNSUPPORTED: libtest option '--ignored' is single-use "
                            f"but was repeated; next: pass --ignored at most once")
                    if include_ignored:
                        raise ValueError(
                            f"UNSUPPORTED: conflicting ignore modes '--ignored' and "
                            f"'--include-ignored' cannot combine; next: pass at most "
                            f"one of --ignored, --include-ignored")
                    ignored_only = True
                elif option == "--include-ignored":
                    if include_ignored:
                        raise ValueError(
                            f"UNSUPPORTED: libtest option '--include-ignored' is single-use "
                            f"but was repeated; next: pass --include-ignored at most once")
                    if ignored_only:
                        raise ValueError(
                            f"UNSUPPORTED: conflicting ignore modes '--ignored' and "
                            f"'--include-ignored' cannot combine; next: pass at most "
                            f"one of --ignored, --include-ignored")
                    include_ignored = True
            elif option in CARGO_KNOWN:
                raise ValueError(
                    f"UNSUPPORTED: known Cargo option {option!r} after `--`; "
                    f"next: move Cargo options before `--` and libtest options after it")
            elif not word.startswith("-"):
                filters.append(word)
            else:
                raise ValueError(
                    f"UNKNOWN_OPTION: unsupported flag {word!r} after `--`; "
                    f"next: use only --exact, --ignored, --include-ignored, --skip, --test-threads")
        elif word == "--":
            libtest = True
        elif option in NOEXEC_OPTIONS:
            raise ValueError(
                f"NOEXEC: refusing non-executing mode {option!r} as automated coverage; "
                f"next: remove {option} so the command actually runs tests")
        elif option in UNSUPPORTED_TARGETS:
            raise ValueError(f"unsupported target selector {option}")
        elif option in LIBTEST_KNOWN:
            raise ValueError(
                f"UNSUPPORTED: known libtest option {option!r} before `--`; "
                f"next: move libtest options after `--`")
        elif option in CARGO_VALUE_OPTIONS:
            if has_value:
                if not value:
                    raise ValueError(
                        f"INVALID_VALUE: option {option!r} got an empty value after `=`; "
                        f"next: give a nonempty value or drop `=value`")
            else:
                value = next(words, None)
                # Cargo value options never take option-like values: a
                # following flag (or `--`) is a missing value, not a
                # consumed token. Scoped to the Cargo side only: the
                # libtest branch must keep accepting dash-prefixed
                # --skip patterns, which getopts permits.
                if value is None or value.startswith("-"):
                    raise ValueError(
                        f"MISSING_VALUE: option {option!r} needs a value but none followed; "
                        f"next: add a value, e.g. `{option} <value>` or `{option}=<value>`")
            if option in ("-p", "--package"):
                names.append(value)
            elif option == "--test":
                selectors.append(("test", value))
        elif option in CARGO_FLAG_OPTIONS:
            if has_value:
                raise ValueError(
                    f"INVALID_VALUE: flag {option!r} takes no value but got {value!r}; "
                    f"next: drop `={value}` and pass {option} alone")
            selectors.append((option, None))
        elif not word.startswith("-"):
            # Pinned Cargo 1.98.1 takes a single [TESTNAME] before `--` and
            # rejects a second positional with unexpected-argument, running
            # nothing. Libtest after `--` accepts several filters.
            if pre_filters >= 1:
                raise ValueError(
                    f"UNSUPPORTED: Cargo accepts at most one positional filter before "
                    f"`--` but got second filter {word!r}; next: move extra filters "
                    f"after `--` where libtest accepts several")
            filters.append(word)
            pre_filters += 1
        else:
            raise ValueError(
                f"UNKNOWN_OPTION: unsupported flag {word!r} before `--`; "
                f"next: use only -p/--package, --test, --lib, --tests, --all-targets, --features")
    if len(names) != 1:
        raise ValueError(f"name exactly one -p package, got {names}")
    if names[0] not in packages:
        raise ValueError(f"unknown package -p {names[0]}; workspace has {sorted(packages)}")
    directory = packages[names[0]]
    targets = crate_targets(directory)
    roots = []
    for selector, name in selectors or [("--tests", None)]:
        if selector == "test":
            if name not in targets["test"]:
                where = (directory / "tests" / f"{name}.rs").relative_to(root)
                raise ValueError(f"missing test target --test {name} ({where})")
            roots.append(targets["test"][name])
        elif selector == "--lib":
            if not targets["lib"]:
                raise ValueError(f"{names[0]} has no library target")
            roots += targets["lib"].values()
        else:
            roots += [*targets["lib"].values(), *targets["bin"].values(),
                      *targets["test"].values()]
    tests = frozenset().union(*map(target_tests, roots))
    if not tests:
        raise ValueError(f"no tests in the selected targets of {names[0]}")
    ignored = frozenset().union(*map(target_ignored_tests, roots)) & tests
    for pattern in filters:
        if not (pattern in tests if exact else any(pattern in name for name in tests)):
            raise ValueError(
                f"no test {'named' if exact else 'matching'} {pattern!r} in the selected targets")
    if filters:
        matched = {candidate for candidate in tests
                   if any(candidate == pattern if exact else pattern in candidate
                          for pattern in filters)}
    else:
        matched = set(tests)
    # Libtest ignore semantics: default runs matched minus ignored,
    # --ignored runs matched intersect ignored, --include-ignored runs all
    # matched. An ignored-only default match or a nonignored-only --ignored
    # match executes zero tests and cannot satisfy coverage.
    if include_ignored:
        executable = set(matched)
    elif ignored_only:
        executable = set(matched) & set(ignored)
    else:
        executable = set(matched) - set(ignored)
    if skips:
        executable -= {candidate for candidate in executable
                       if any(candidate == skip if exact else skip in candidate
                              for skip in skips)}
    if not executable:
        raise ValueError(
            f"EMPTY_SELECTION: no effective tests remain after filters {filters!r} with "
            f"--exact={exact}, --ignored={ignored_only}, --include-ignored={include_ignored} "
            f"and --skip {skips!r}; next: narrow --skip, adjust ignore mode, or add a "
            f"positive filter that selects a real executable test")


def check_verification(entry_id, verification, root=ROOT):
    """Offline guard: every Cargo invocation of an automated row resolves."""
    try:
        commands = cargo_commands(verification)
        if not commands:
            raise ValueError("no cargo test invocation")
        packages = workspace_packages(root)
        for args in commands:
            check_cargo_test(args, root, packages)
    except (ValueError, OSError) as error:
        raise ValueError(f"{entry_id}: {error}") from error


def strings(value):
    """Every string in a checklist entry, except the historical exemptions."""
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from strings(item)
    elif isinstance(value, dict):
        for key, item in value.items():
            if key != "historical":
                yield from strings(item)


def script_exists(path, root):
    """True when a cited script path is a file inside the repository."""
    target = (root / path).resolve()
    return target.is_file() and target.is_relative_to(root.resolve())


def check_script_refs(entry, root=ROOT):
    """Offline guard: operative script paths in an entry must exist unless marked historical."""
    exempt = {}
    for item in entry.get("historical", []):
        if not all(isinstance(item.get(k), str) and item[k].strip() for k in ("path", "reason")):
            raise ValueError(f"{entry['id']}: historical script reference requires path and reason")
        exempt[item["path"]] = item["reason"]
    cited = {path for text in strings(entry) for path in SCRIPT_REF.findall(text)}
    for path in sorted(exempt):
        if path not in cited or script_exists(path, root):
            raise ValueError(f"{entry['id']}: stale historical exemption {path}")
    for path in sorted(cited - set(exempt)):
        if not script_exists(path, root):
            raise ValueError(f"{entry['id']}: missing script {path} is cited as operative")


def validate(markdown, checklist, root=ROOT):
    owners = delta_rows(markdown)
    expected = set(parity_rows(markdown)) | set(owners)
    if checklist.get("schema_version") != 1:
        raise ValueError("Unsupported checklist schema_version")
    entries = checklist.get("entries", [])
    actual = []
    ids = []
    for entry in entries:
        ids.append(entry["id"])
        source = entry["parity"]
        key = (source["section"], tuple(source["row"]))
        actual.append(key)
        if entry["status"] not in ("automated", "manual", "waived"):
            raise ValueError(f"{entry['id']}: invalid status")
        for field in ("id", "action", "expected", "evidence"):
            if not isinstance(entry.get(field), str) or not entry[field].strip():
                raise ValueError(f"{entry.get('id')}: missing {field}")
        if entry["status"] == "waived" and not entry.get("reason", "").strip():
            raise ValueError(f"{entry['id']}: waiver requires reason")
        if entry["status"] == "waived" and not entry.get("approver", "").strip():
            raise ValueError(f"{entry['id']}: waiver requires approver")
        if entry["status"] == "automated" and not entry.get("verification", "").strip():
            raise ValueError(f"{entry['id']}: automated requires verification")
        if entry["status"] == "automated":
            check_verification(entry["id"], entry["verification"], root)
        check_script_refs(entry, root)
        # Voice-sensitive rows are cross-links, never a second voice procedure.
        voice = any("voice" in c.lower() for c in key[1])
        voice = voice or (key[0] == 3 and "ShardResume" in key[1][0])
        if voice and "TOG-10119" not in entry.get("reference", ""):
            raise ValueError(f"{entry['id']}: voice coverage must reference TOG-10119")
        if key[0] in DELTA_SECTIONS:
            owner = entry.get("owner")
            if (not isinstance(owner, list) or not owner
                    or not all(isinstance(card, str) and CARD.fullmatch(card) for card in owner)):
                raise ValueError(f"{entry['id']}: §{key[0]} entry requires owner cards")
            # The owner list tracks the disposition; a moved card is stale.
            if owners.get(key) and tuple(owner) != owners[key]:
                raise ValueError(
                    f"{entry['id']}: stale owner {owner}; parity names {list(owners[key])}")
    if len(ids) != len(set(ids)) or len(actual) != len(set(actual)):
        raise ValueError("Duplicate checklist IDs or parity rows")
    missing = expected - set(actual)
    stale = set(actual) - expected
    if missing or stale:
        raise ValueError(f"Coverage mismatch: missing={sorted(missing)!r}; stale={sorted(stale)!r}")
    return Counter(section for section, _ in expected)


def render(checklist):
    lines = ["# Parity soak checklist", "", "<!-- Generated from soak-checklist.json by scripts/check_soak_checklist.py --render. -->", ""]
    lines += checklist["instructions"] + [""]
    for section in SECTIONS:
        lines += [f"## {section}. {checklist['sections'][str(section)]}", ""]
        for entry in checklist["entries"]:
            if entry["parity"]["section"] != section:
                continue
            row = entry["parity"]["row"]
            # §12 cites evidence links and §13 an area list; label by name.
            if section == 12:
                label = unlink(row[0])
            elif section == 13:
                label = f"{unlink(row[0])} — {row[2]}"
            else:
                label = " — ".join(row[:2])
            lines += [f"### {entry['id']}: {label}", "",
                      f"- **Method:** `{entry['status']}` (not an execution verdict).",
                      f"- **Action:** {entry['action']}",
                      f"- **Expected:** {entry['expected']}",
                      f"- **Evidence:** {entry['evidence']}"]
            if entry.get("owner"):
                lines.append("- **Owner:** " + ", ".join(
                    f"[{card}](/TOG/issues/{card})" for card in entry["owner"]))
            for field in ("verification", "reason", "approver", "reference"):
                if entry.get(field):
                    lines.append(f"- **{field.title()}:** {entry[field]}")
            for item in entry.get("historical", []):
                lines.append(f"- **Historical:** `{item['path']}` — {item['reason']}")
            lines.append("")
    text = "\n".join(lines)
    toc = ["<!-- start-here -->", "**Start here — section map**", ""]
    for heading in re.findall(r"^## (.+)$", text, re.M):
        anchor = re.sub(r"\s", "-", re.sub(r"[^\w\s-]", "", heading.lower()))
        toc.append(f"- [{heading}](#{anchor})")
    toc += ["<!-- /start-here -->", ""]
    return text.replace("# Parity soak checklist\n\n",
                        "# Parity soak checklist\n\n" + "\n".join(toc) + "\n", 1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--render", action="store_true", help="Print the generated Markdown")
    args = parser.parse_args()
    checklist = json.loads((ROOT / "docs/soak-checklist.json").read_text())
    try:
        counts = validate((ROOT / "docs/parity.md").read_text(), checklist)
        output = render(checklist)
        if args.render:
            print(output, end="")
        else:
            if (ROOT / "docs/soak-checklist.md").read_text() != output:
                raise ValueError("Markdown drift: regenerate with --render")
            print(f"PASS: {sum(counts.values())}/{sum(counts.values())} non-DROP rows; "
                  + ", ".join(f"§{s}={counts[s]}" for s in sorted(counts)))
    except (ValueError, KeyError, TypeError) as error:
        parser.exit(1, f"FAIL: {error}\n")


if __name__ == "__main__":
    main()
