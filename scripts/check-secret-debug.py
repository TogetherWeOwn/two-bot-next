#!/usr/bin/env python3
"""Grep-style tripwire for raw credential fields under derived Rust Debug.

Not a Rust parser or data-flow analysis: review new secret-bearing types and
explicit expose() calls too. Comments/literals are blanked to preserve lines.
"""
import argparse
import contextlib
import io
import json
import os
import pathlib
import re
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
# Coverage floors: the guard fails instead of passing vacuously when the scan
# sees fewer files or Debug-derived types than this. Measured 464 files and
# 1169 Debug-derived types on 2026-10-21; re-baseline deliberately (never to
# make a red guard green) when the tree shrinks for good.
MIN_SCANNED_FILES = 400
MIN_DEBUG_DERIVED_TYPES = 1000
# The attribute run may hold several derive attributes (in either order, with
# other attributes between them); Debug in ANY of them counts. Doc comments
# are already blanked to whitespace before matching, so plain `\s*` covers
# the gaps between the bracketed attributes.
DERIVE = re.compile(
    r"(?P<attrs>(?:#\[[^\]]*\]\s*)+)"
    r"(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum)\s+(?P<name>\w+)[^{;]*\{"
)
DERIVE_ONE = re.compile(r"#\[derive\((?P<traits>[^)]*)\)\]")
FIELD = re.compile(r"\b(\w+)\s*:(?!:)\s*")
# Tuple variants carry no field name, so key off an exactly
# credential-bearing variant name (a claim token is an ownership
# credential in this codebase). Anchored: `SecretTooShort(String)`
# carries a key id, not a secret, and must not trip this.
TUPLE_VARIANT = re.compile(r"\b([A-Z]\w*)\s*\(")
SENSITIVE_VARIANT = re.compile(
    r"(?:claimed?|claimtoken|token|accesstoken|refreshtoken|secret|password|credentials?)",
    re.IGNORECASE,
)
SENSITIVE = re.compile(
    r"(?:^|_)(?:token|secret|password|credential|credentials)(?:_|$)"
    r"|^(?:database_url|webhook_url|access_key_id|secret_access_key)$"
)
RAW = re.compile(r"\b(?:String|str|u8)\b")
# Public one-way correlation hashes, NOT credentials. Exceptions are exact,
# audited type.field pairs, never a wildcard path or generic token exemption.
EXCEPTIONS = {
    ("crates/core/src/mac.rs", "ModerationMarker", "token"):
        "one-way moderation correlation hash, no authentication authority",
    ("crates/core/src/funnel.rs", "FunnelEvent", "dedupe_token"):
        "funnel idempotency identifier, not an authentication credential",
    ("crates/core/src/handlers.rs", "StoredRow", "dedupe_token"):
        "funnel idempotency identifier, not an authentication credential",
    ("crates/core/src/voice_naming.rs", "Segment", "Token"):
        "voice name-template token name (lowercased @@name@@), not a credential",
}


def tuple_variants(body):
    """Yield (name, payload, offset) for tuple variants in `body`.

    The payload is captured with balanced-paren scanning: a non-greedy
    regex stops at the first inner `)`, so fields after a parenthesized
    field (e.g. `Claimed((u64, u64), String)`) would never be checked.
    """
    for match in TUPLE_VARIANT.finditer(body):
        depth, end = 1, match.end()
        while end < len(body) and depth:
            depth += (body[end] == "(") - (body[end] == ")")
            end += 1
        if depth:
            continue
        yield match.group(1), body[match.end():end - 1], match.start()


def strip_secret_spans(text):
    """Remove balanced `Secret<...>` spans, leaving any raw types behind.

    A per-field `Secret<` anywhere check is fail-open when a raw type is
    nested in the same top-level field as a `Secret`: e.g.
    `(Secret<String>, String)`, `Option<(Secret<String>, String)>` and
    `fn() -> (Secret<String>, String)` all print the raw element under
    derived Debug. Stripping the balanced spans (the `>` of a `->`
    inside the span does not close it) exposes such raw remnants.
    """
    out, i = [], 0
    while True:
        match = re.search(r"\bSecret\s*<", text[i:])
        if not match:
            out.append(text[i:])
            return "".join(out)
        start = i + match.start()
        out.append(text[i:start])
        depth, j = 1, i + match.end()
        while j < len(text) and depth:
            if text[j] == "<":
                depth += 1
            elif text[j] == ">" and not (j > 0 and text[j - 1] == "-"):
                depth -= 1
            j += 1
        i = j


def has_raw_outside_secret(text):
    return bool(RAW.search(strip_secret_spans(text)))


def split_top_level(payload):
    fields, depth, start = [], 0, 0
    for i, ch in enumerate(payload):
        if ch in "(<[{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == ">":
            # The `>` of a `->` return arrow is not a closing angle
            # bracket: counting it drives depth to -1, the following
            # top-level comma is missed, and a raw field can hide
            # behind an earlier `Secret<...>`. Clamp stray `>` at 0
            # for the same reason.
            if i > 0 and payload[i - 1] == "-":
                continue
            if depth > 0:
                depth -= 1
        elif ch == "," and depth == 0:
            fields.append(payload[start:i])
            start = i + 1
    fields.append(payload[start:])
    return fields


def named_fields(body):
    """Yield (name, type, offset) for named fields in `body`.

    The type is captured with balanced-bracket scanning: truncating at
    the first comma (the old `[^,\\n}]+`) hides every raw element after
    it, e.g. `token: (u64, String)` or `token: (Secret<String>, String)`.
    Scanning stops at a top-level comma, semicolon, or closing bracket.
    """
    for match in FIELD.finditer(body):
        depth, end = 0, match.end()
        while end < len(body):
            ch = body[end]
            if ch in "(<[{":
                depth += 1
            elif ch in ")]}":
                if depth == 0:
                    break
                depth -= 1
            elif ch == ">":
                if end > 0 and body[end - 1] == "-":
                    pass  # return arrow, not a closing angle bracket
                elif depth > 0:
                    depth -= 1
            elif ch in ",;" and depth == 0:
                break
            end += 1
        yield match.group(1), body[match.end():end], match.start()


def blank_literals(source):
    pattern = r'//[^\n]*|/\*[\s\S]*?\*/|r(#+)?"[\s\S]*?"\1|"(?:\\.|[^"\\])*"'
    return re.sub(pattern, lambda m: re.sub(r"[^\n]", " ", m.group()), source)


def violations(source, path="fixture.rs", _stats=None, exceptions=None):
    """Raw credential fields under derived Debug in one file.

    When `_stats` is given, count each Debug-derived type in
    `_stats["debug_types"]` and record every exercised exception key in
    `_stats["used"]`, so the caller can floor coverage and spot stale
    allowlist entries. `exceptions` overrides EXCEPTIONS (tests only).
    """
    active = EXCEPTIONS if exceptions is None else exceptions
    clean = blank_literals(source)
    results = []
    for match in DERIVE.finditer(clean):
        traits = ",".join(
            derive["traits"] for derive in DERIVE_ONE.finditer(match["attrs"])
        )
        if "Debug" not in traits.split(",") and not re.search(r"\bDebug\b", traits):
            continue
        if _stats is not None:
            _stats["debug_types"] += 1
        start = match.end()
        end, depth = start, 1
        while end < len(clean) and depth:
            depth += (clean[end] == "{") - (clean[end] == "}")
            end += 1
        body = clean[start:end - 1]
        for name, field_type, field_start in named_fields(body):
            if not SENSITIVE.search(name) or not has_raw_outside_secret(field_type):
                continue
            if (path, match["name"], name) in active:
                if _stats is not None:
                    _stats["used"].add((path, match["name"], name))
                continue
            line = source.count("\n", 0, start + field_start) + 1
            results.append(f"{path}:{line}: {match['name']}.{name} is raw under derived Debug; use Secret<T>")
        for name, payload, variant_start in tuple_variants(body):
            if not SENSITIVE_VARIANT.fullmatch(name):
                continue
            if not any(
                has_raw_outside_secret(field)
                for field in split_top_level(payload)
            ):
                continue
            if (path, match["name"], name) in active:
                if _stats is not None:
                    _stats["used"].add((path, match["name"], name))
                continue
            line = source.count("\n", 0, start + variant_start) + 1
            results.append(f"{path}:{line}: {match['name']}::{name} is raw under derived Debug; use Secret<T>")
    return results


def scan_paths(root):
    """Crate sources plus top-level src/, mirroring the snowflake gate."""
    paths = []
    if (root / "crates").is_dir():
        paths.extend((root / "crates").glob("*/src/**/*.rs"))
    if (root / "src").is_dir():
        paths.extend((root / "src").glob("**/*.rs"))
    return sorted(set(paths))


def check(root=ROOT, min_files=MIN_SCANNED_FILES,
          min_debug=MIN_DEBUG_DERIVED_TYPES, exceptions=None):
    """Scan `root` and grade coverage: findings, floors and stale exceptions."""
    active = EXCEPTIONS if exceptions is None else exceptions
    stats = {"debug_types": 0, "used": set()}
    findings = []
    paths = scan_paths(root)
    for path in paths:
        findings.extend(violations(path.read_text(), path.relative_to(root).as_posix(),
                                   _stats=stats, exceptions=active))
    stale = sorted(set(active) - stats["used"])
    errors = []
    if len(paths) < min_files:
        errors.append(f"secret Debug guard: scanned {len(paths)} files, "
                      f"below the {min_files}-file floor")
    if stats["debug_types"] < min_debug:
        errors.append(f"secret Debug guard: found {stats['debug_types']} "
                      f"Debug-derived types, below the {min_debug}-type floor")
    for entry in stale:
        errors.append(f"stale exception {entry[0]} {entry[1]} {entry[2]}: "
                      f"no longer matches a raw credential field")
    return {
        "scanned_files": len(paths),
        "debug_types": stats["debug_types"],
        "exceptions_total": len(active),
        "exceptions_used": sorted(stats["used"]),
        "stale_exceptions": stale,
        "findings": findings,
        "errors": errors,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test", action="store_true",
                        help="run the guard's unit tests")
    parser.add_argument("--json", action="store_true",
                        help="print counts and findings as JSON")
    parser.add_argument("--root", type=pathlib.Path, default=ROOT,
                        help="repository root to scan")
    args = parser.parse_args(argv)
    if args.test:
        unittest.main(argv=[sys.argv[0]])
    result = check(args.root)
    failed = bool(result["findings"] or result["errors"])
    if args.json:
        print(json.dumps({
            "scanned_files": result["scanned_files"],
            "debug_derived_types": result["debug_types"],
            "exceptions_total": result["exceptions_total"],
            "exceptions_used": len(result["exceptions_used"]),
            "stale_exceptions": [list(entry) for entry in result["stale_exceptions"]],
            "findings": result["findings"],
            "errors": result["errors"],
            "verdict": "FAIL" if failed else "PASS",
        }, indent=2))
    else:
        for finding in result["findings"]:
            print(finding, file=sys.stderr)
        for error in result["errors"]:
            print(error, file=sys.stderr)
        if not failed:
            print("secret Debug guard: no raw credential fields under derived Debug")
    return 1 if failed else 0


class GuardTests(unittest.TestCase):
    def test_raw_strings_and_bytes_fail(self):
        for field in ["token: String", "pub database_url: Option<String>", "secret: Vec<u8>", "hmac_secret: [u8; 32]", "webhook_url: String"]:
            for kind in ["struct", "enum"]:
                code = f"#[derive(Clone, Debug)]\npub {kind} Fixture {{ {field}, }}"
                self.assertEqual(len(violations(code)), 1, code)

    def test_borrowed_strings_fail(self):
        for field in ["token: &'a str", "secret: &str", "password: Option<&'a str>", "webhook_url: Vec<&str>"]:
            code = f"#[derive(Debug)] struct Credentials<'a> {{ {field}, }}"
            self.assertEqual(len(violations(code)), 1, code)

    def test_multiline_derive_and_extra_attributes_fail(self):
        self.assertTrue(violations('#[derive(\n Debug,\n Clone\n)]\n#[serde(default)]\npub(crate) struct Fixture { pub discord_token: String, }'))

    def test_split_derive_attributes_fail_in_either_order(self):
        for attrs in [
            '#[derive(Clone)]\n#[derive(Debug)]',
            '#[derive(Debug)]\n#[derive(Clone)]',
            '#[derive(Clone)]\n#[serde(default)]\n#[derive(Debug)]',
            '/// doc comment\n#[derive(Clone)]\n// line comment\n#[derive(Debug)]',
        ]:
            code = f'{attrs}\nstruct Credentials {{ token: String, }}'
            self.assertEqual(len(violations(code)), 1, code)

    def test_split_derive_without_debug_passes(self):
        self.assertFalse(violations('#[derive(Clone)]\n#[derive(Default)]\nstruct Safe { token: String, }'))

    def test_wrappers_and_custom_debug_pass(self):
        self.assertFalse(violations('#[derive(Debug)] struct Safe { token: Option<Secret<String>>, secret: crate::Secret<Vec<u8>>, }'))
        self.assertFalse(violations('#[derive(Clone)] struct Safe { token: String, }'))

    def test_comments_literals_and_nonsecret_fields_pass(self):
        self.assertFalse(violations('// #[derive(Debug)] struct Bad { token: String }\n#[derive(Debug)] struct Fine { token_count: usize, id: String }'))
        self.assertFalse(violations('const TEXT: &str = "#[derive(Debug)] struct Bad { token: String }";'))

    def test_tuple_variants_fail(self):
        for variant in ["Claimed(String)", "Token(String)", "ClaimToken(Vec<u8>)", "Secret(&'a str)"]:
            code = f"#[derive(Clone, Debug)]\npub enum Fixture {{ {variant}, Other, }}"
            self.assertEqual(len(violations(code)), 1, code)

    def test_tuple_secret_wrapper_and_plain_payloads_pass(self):
        self.assertFalse(violations('#[derive(Debug)] enum Fixture { Claimed(Secret<String>), Token(crate::Secret<String>), }'))
        self.assertFalse(violations('#[derive(Debug)] enum Fixture { Message(String), Count(u64), }'))
        self.assertFalse(violations('#[derive(Debug)] enum Fixture { Claimed(Secret<[u8; 32]>, u64), }'))
        self.assertFalse(violations('#[derive(Debug)] enum Fixture { Claimed(Option<Secret<String>>, u64), }'))
        self.assertFalse(violations('#[derive(Debug)] enum Fixture { Claimed((u64, u64), Secret<String>), }'))
        self.assertFalse(violations('#[derive(Debug)] enum Fixture { Token(fn() -> Secret<String>), }'))

    def test_tuple_mixed_and_array_payloads_fail(self):
        for variant in [
            "Token(String, Secret<String>)",
            "Token(Secret<String>, String)",
            "ClaimToken([u8; 32])",
            "Claimed([u8; 32])",
            "Token(Secret<String>, [u8; 32])",
            "Claimed((u64, u64), String)",
            "Token((u64,u64), Vec<u8>)",
            "Token(fn() -> Secret<String>, String)",
            "Token(Box<dyn Fn() -> Secret<String>>, String)",
            "Claimed((Secret<String>, String))",
            "Token(Option<(Secret<String>, String)>)",
            "Token(fn() -> (Secret<String>, String))",
        ]:
            code = f"#[derive(Debug)] enum Fixture {{ {variant}, Other, }}"
            self.assertEqual(len(violations(code)), 1, code)

    def test_arrow_inside_secret_at_depth_passes(self):
        # The `>` of `->` at depth > 0 must not close a bracket: without
        # that guard the payload splits/strips at the arrow and the raw
        # `String` inside the `Secret<...>` span looks unwrapped.
        self.assertFalse(violations('#[derive(Debug)] enum Fixture { Token(Secret<fn() -> u64, String>), }'))
        self.assertFalse(violations('#[derive(Debug)] enum Fixture { Token(Secret<fn() -> String>), }'))

    def test_named_fields_after_first_comma_fail(self):
        for field in [
            "token: (u64, String)",
            "token: (Secret<String>, String)",
            "token: Option<(Secret<String>, String)>",
            "password: HashMap<u64, String>",
            "token: fn() -> (Secret<String>, String)",
        ]:
            code = f"#[derive(Debug)] struct Fixture {{ {field}, }}"
            self.assertEqual(len(violations(code)), 1, code)
        code = "#[derive(Debug)] enum Fixture { V { token: (u64, String) }, }"
        self.assertEqual(len(violations(code)), 1, code)

    def test_named_wrapped_fields_after_first_comma_pass(self):
        self.assertFalse(violations('#[derive(Debug)] struct Safe { token: (Secret<String>, u64), }'))
        self.assertFalse(violations('#[derive(Debug)] struct Safe { token: (u64, Secret<String>), }'))

    def test_exception_is_exact(self):
        code = '#[derive(Debug)] struct ModerationMarker { token: String }'
        self.assertFalse(violations(code, "crates/core/src/mac.rs"))
        self.assertTrue(violations(code, "other.rs"))
        code = '#[derive(Debug)] enum Segment { Token(String) }'
        self.assertFalse(violations(code, "crates/core/src/voice_naming.rs"))
        self.assertTrue(violations(code, "other.rs"))

    def test_floors_are_the_documented_contract(self):
        self.assertEqual(MIN_SCANNED_FILES, 400)
        self.assertEqual(MIN_DEBUG_DERIVED_TYPES, 1000)
        self.assertEqual(len(EXCEPTIONS), 4)

    def test_scan_covers_crates_and_top_level_src(self):
        root = _scratch_root(self)
        bad = '#[derive(Debug)] struct Bad { token: String, }\n'
        (root / "crates" / "demo" / "src").mkdir(parents=True)
        (root / "crates" / "demo" / "src" / "bad.rs").write_text(bad)
        (root / "src").mkdir()
        (root / "src" / "top.rs").write_text(bad)
        result = check(root, min_files=0, min_debug=0, exceptions={})
        self.assertEqual(result["scanned_files"], 2)
        self.assertEqual(result["debug_types"], 2)
        self.assertEqual(len(result["findings"]), 2)
        self.assertTrue(any("crates/demo/src/bad.rs" in f for f in result["findings"]))
        self.assertTrue(any("src/top.rs" in f for f in result["findings"]))
        self.assertEqual(result["errors"], [])

    def test_empty_tree_fails_the_file_floor(self):
        root = _scratch_root(self)
        result = check(root, min_files=1, min_debug=0, exceptions={})
        self.assertEqual(result["scanned_files"], 0)
        self.assertEqual(result["findings"], [])
        self.assertEqual(len(result["errors"]), 1)
        self.assertIn("0 files", result["errors"][0])
        self.assertIn("floor", result["errors"][0])

    def test_floors_are_inclusive_bounds(self):
        root = _scratch_root(self)
        (root / "src").mkdir()
        (root / "src" / "one.rs").write_text('#[derive(Debug)] struct One { id: u64, }\n')
        at = check(root, min_files=1, min_debug=1, exceptions={})
        self.assertEqual(at["errors"], [])
        over = check(root, min_files=2, min_debug=2, exceptions={})
        self.assertEqual(len(over["errors"]), 2)

    def test_renamed_exception_entry_is_stale(self):
        cases = [
            # (relpath, type, field, file body or None for a missing file)
            ("crates/demo/src/w.rs", "Widget", "token",
             '#[derive(Debug)] struct Widget { token_id: u64, }\n'),
            ("crates/demo/src/w.rs", "Widget", "token",
             '#[derive(Debug)] struct Gadget { token: String, }\n'),
            ("crates/demo/src/gone.rs", "Widget", "token", None),
        ]
        for relpath, typename, field, body in cases:
            with self.subTest(relpath=relpath, body=body):
                root = _scratch_root(self)
                if body is not None:
                    target = root / relpath
                    target.parent.mkdir(parents=True)
                    target.write_text(body)
                result = check(root, min_files=0, min_debug=0,
                               exceptions={(relpath, typename, field): "audited"})
                self.assertEqual(result["stale_exceptions"], [(relpath, typename, field)])
                self.assertEqual(len(result["errors"]), 1)
                self.assertIn("stale exception", result["errors"][0])

    def test_live_exception_is_used_not_stale(self):
        root = _scratch_root(self)
        (root / "src").mkdir()
        (root / "src" / "w.rs").write_text(
            '#[derive(Debug)] struct Widget { token: String, }\n')
        key = ("src/w.rs", "Widget", "token")
        result = check(root, min_files=0, min_debug=0, exceptions={key: "audited"})
        self.assertEqual(result["findings"], [])
        self.assertEqual(result["stale_exceptions"], [])
        self.assertEqual(result["exceptions_used"], [key])
        self.assertEqual(result["errors"], [])

    def test_secret_wrapped_exception_goes_stale(self):
        # The exception is no longer needed once the field is wrapped: the
        # allowlist must shrink with the code instead of rotting.
        root = _scratch_root(self)
        (root / "src").mkdir()
        (root / "src" / "w.rs").write_text(
            '#[derive(Debug)] struct Widget { token: Secret<String>, }\n')
        key = ("src/w.rs", "Widget", "token")
        result = check(root, min_files=0, min_debug=0, exceptions={key: "audited"})
        self.assertEqual(result["stale_exceptions"], [key])

    def test_json_counts(self):
        root = _scratch_root(self)
        (root / "src").mkdir()
        (root / "src" / "w.rs").write_text(
            '#[derive(Debug)] struct Widget { token: String, }\n')
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = main(["--root", str(root), "--json"])
        self.assertEqual(code, 1)
        packet = json.loads(out.getvalue())
        self.assertEqual(packet["scanned_files"], 1)
        self.assertEqual(packet["debug_derived_types"], 1)
        self.assertEqual(packet["exceptions_used"], 0)
        self.assertEqual(packet["verdict"], "FAIL")
        self.assertEqual(len(packet["findings"]), 1)

    def test_human_output_keeps_the_pass_line(self):
        # Against the real tree: pins the production floors as holding and
        # the pass line byte-identical.
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = main(["--root", str(ROOT)])
        self.assertEqual(code, 0, err.getvalue())
        self.assertEqual(out.getvalue(),
                         "secret Debug guard: no raw credential fields under derived Debug\n")
        self.assertEqual(err.getvalue(), "")


def _scratch_root(test):
    tmp = tempfile.TemporaryDirectory(
        dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
        or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        or os.environ.get("RUNNER_TEMP"))
    test.addCleanup(tmp.cleanup)
    return pathlib.Path(tmp.name)


if __name__ == "__main__":
    raise SystemExit(main())
