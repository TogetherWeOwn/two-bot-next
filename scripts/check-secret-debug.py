#!/usr/bin/env python3
"""Grep-style tripwire for raw credential fields under derived Rust Debug.

Not a Rust parser or data-flow analysis: review new secret-bearing types and
explicit expose() calls too. Comments/literals are blanked to preserve lines.
"""
import pathlib
import re
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
DERIVE = re.compile(
    r"#\[derive\((?P<traits>[^)]*)\)\]"
    r"(?:\s|#\[[^\]]*\])*"
    r"(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum)\s+(?P<name>\w+)[^{;]*\{"
)
FIELD = re.compile(r"\b(\w+)\s*:\s*([^,\n}]+)")
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
}


def blank_literals(source):
    pattern = r'//[^\n]*|/\*[\s\S]*?\*/|r(#+)?"[\s\S]*?"\1|"(?:\\.|[^"\\])*"'
    return re.sub(pattern, lambda m: re.sub(r"[^\n]", " ", m.group()), source)


def violations(source, path="fixture.rs"):
    clean = blank_literals(source)
    results = []
    for match in DERIVE.finditer(clean):
        if "Debug" not in match["traits"].split(",") and not re.search(r"\bDebug\b", match["traits"]):
            continue
        start = match.end()
        end, depth = start, 1
        while end < len(clean) and depth:
            depth += (clean[end] == "{") - (clean[end] == "}")
            end += 1
        body = clean[start:end - 1]
        for field in FIELD.finditer(body):
            name, field_type = field.groups()
            if not SENSITIVE.search(name) or not RAW.search(field_type):
                continue
            if re.search(r"\bSecret\s*<", field_type):
                continue
            if (path, match["name"], name) in EXCEPTIONS:
                continue
            line = source.count("\n", 0, start + field.start()) + 1
            results.append(f"{path}:{line}: {match['name']}.{name} is raw under derived Debug; use Secret<T>")
    return results


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

    def test_wrappers_and_custom_debug_pass(self):
        self.assertFalse(violations('#[derive(Debug)] struct Safe { token: Option<Secret<String>>, secret: crate::Secret<Vec<u8>>, }'))
        self.assertFalse(violations('#[derive(Clone)] struct Safe { token: String, }'))

    def test_comments_literals_and_nonsecret_fields_pass(self):
        self.assertFalse(violations('// #[derive(Debug)] struct Bad { token: String }\n#[derive(Debug)] struct Fine { token_count: usize, id: String }'))
        self.assertFalse(violations('const TEXT: &str = "#[derive(Debug)] struct Bad { token: String }";'))

    def test_exception_is_exact(self):
        code = '#[derive(Debug)] struct ModerationMarker { token: String }'
        self.assertFalse(violations(code, "crates/core/src/mac.rs"))
        self.assertTrue(violations(code, "other.rs"))


if __name__ == "__main__":
    if "--test" in sys.argv:
        unittest.main(argv=[sys.argv[0]])
    else:
        findings = []
        for path in sorted((ROOT / "crates").glob("*/src/**/*.rs")):
            findings.extend(violations(path.read_text(), path.relative_to(ROOT).as_posix()))
        for finding in findings:
            print(finding, file=sys.stderr)
        if findings:
            sys.exit(1)
        print("secret Debug guard: no raw credential fields under derived Debug")
