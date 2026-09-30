#!/usr/bin/env python3
"""Reject deployment snowflake literals in production Rust source.

Comments and exact #[cfg(test)] inline modules are not executable source. The
allowlist pins existing parity/identity guards by path AND complete source line;
new values, uses, or duplicates require an explicit, justified allowlist change.
This is a lexical regression gate, not a Rust constant-expression evaluator.
"""

import argparse
from collections import Counter
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[2]
SNOWFLAKE = re.compile(r"(?<![\w])\d[\d_]*(?![\d_])")
TOKEN = re.compile(
    r'(?P<raw>(?:br|r)(?P<hashes>\#*)".*?"(?P=hashes))'
    r'|(?P<string>b?"(?:\\.|[^"\\])*")'
    r"|(?P<char>b?'(?:\\.|[^'\\])')"
    r"|(?P<word>[A-Za-z_][A-Za-z_0-9]*)"
    r"|(?P<number>\d[\d_]*(?:[A-Za-z][A-Za-z_0-9]*)?)"
    r"|(?P<punct>\S)",
    re.DOTALL,
)


def tokens(source):
    """Tokenize literals and punctuation without treating strings as comments."""
    pos = 0
    while pos < len(source):
        if source.startswith("//", pos):
            end = source.find("\n", pos)
            pos = len(source) if end < 0 else end
        elif source.startswith("/*", pos):
            depth = 1
            pos += 2
            while pos < len(source) and depth:
                if source.startswith("/*", pos):
                    depth += 1
                    pos += 2
                elif source.startswith("*/", pos):
                    depth -= 1
                    pos += 2
                else:
                    pos += 1
        elif source[pos].isspace():
            pos += 1
        else:
            match = TOKEN.match(source, pos)
            yield match
            pos = match.end()


def production_literals(source):
    items = list(tokens(source))
    test_attribute = ["#", "[", "cfg", "(", "test", ")", "]"]
    i = 0
    while i < len(items):
        if [t.group() for t in items[i:i + 7]] == test_attribute:
            # Only skip an inline module directly gated by cfg(test), allowing
            # intervening attributes/visibility. Never skip the rest of a file.
            j = i + 7
            while j < len(items) and items[j].group() == "#":
                j += 1
                depth = 0
                while j < len(items):
                    text = items[j].group()
                    depth += (text == "[") - (text == "]")
                    j += 1
                    if depth == 0:
                        break
            if j < len(items) and items[j].group() == "pub":
                j += 1
            if j + 2 < len(items) and items[j].group() == "mod" and items[j + 2].group() == "{":
                j += 3
                depth = 1
                while j < len(items) and depth:
                    # String tokens containing braces are never punctuation.
                    text = items[j].group() if items[j].lastgroup == "punct" else ""
                    depth += (text == "{") - (text == "}")
                    j += 1
                i = j
                continue
        item = items[i]
        if item.lastgroup in {"raw", "string", "number"}:
            yield item
        i += 1


def occurrences(source):
    result = []
    lines = source.splitlines()
    for token in production_literals(source):
        for match in SNOWFLAKE.finditer(token.group()):
            value = match.group().replace("_", "")
            if 17 <= len(value) <= 20:
                offset = token.start() + match.start()
                line = source.count("\n", 0, offset) + 1
                result.append((line, value, lines[line - 1].strip()))
    return result


def check(root, allowlist):
    expected = Counter()
    for entry in allowlist:
        if not entry.get("reason", "").strip():
            raise ValueError("each snowflake allowance needs a reason")
        path = entry["path"]
        if not path.startswith("crates/") or "/src/" not in path or ".." in Path(path).parts:
            raise ValueError(f"invalid source allowance path: {path}")
        expected[(path, entry["value"], entry["source"])] += 1
    seen = Counter()
    errors = []
    for path in sorted((root / "crates").glob("*/src/**/*.rs")):
        relative = path.relative_to(root).as_posix()
        for line, value, source in occurrences(path.read_text()):
            key = (relative, value, source)
            seen[key] += 1
            if seen[key] > expected[key]:
                errors.append(f"{relative}:{line}: unallowlisted snowflake literal {value}")
    for key, count in (expected - seen).items():
        errors.append(f"{key[0]}: stale allowance for {key[1]} ({count} missing)")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    try:
        allowlist = json.loads((args.root / "scripts/ci/snowflakes-allowlist.json").read_text())
        errors = check(args.root, allowlist)
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print(f"snowflake gate: {exc}", file=sys.stderr)
        return 1
    for error in errors:
        print(error, file=sys.stderr)
    if not errors:
        print("snowflake gate: no unallowlisted production literals")
    return bool(errors)


if __name__ == "__main__":
    sys.exit(main())
