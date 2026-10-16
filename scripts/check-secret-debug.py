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
# The attribute run may hold several derive attributes (in either order, with
# other attributes between them); Debug in ANY of them counts. Doc comments
# are already blanked to whitespace before matching, so plain `\s*` covers
# the gaps between the bracketed attributes.
DERIVE = re.compile(
    r"(?P<attrs>(?:#\[[^\]]*\]\s*)+)"
    r"(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum)\s+(?P<name>\w+)[^{;]*\{"
)
DERIVE_ONE = re.compile(r"#\[derive\((?P<traits>[^)]*)\)\]")
FIELD = re.compile(r"\b(\w+)\s*:\s*([^,\n}]+)")
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


def blank_literals(source):
    pattern = r'//[^\n]*|/\*[\s\S]*?\*/|r(#+)?"[\s\S]*?"\1|"(?:\\.|[^"\\])*"'
    return re.sub(pattern, lambda m: re.sub(r"[^\n]", " ", m.group()), source)


def violations(source, path="fixture.rs"):
    clean = blank_literals(source)
    results = []
    for match in DERIVE.finditer(clean):
        traits = ",".join(
            derive["traits"] for derive in DERIVE_ONE.finditer(match["attrs"])
        )
        if "Debug" not in traits.split(",") and not re.search(r"\bDebug\b", traits):
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
        for name, payload, variant_start in tuple_variants(body):
            if not SENSITIVE_VARIANT.fullmatch(name):
                continue
            if not any(
                RAW.search(field) and not re.search(r"\bSecret\s*<", field)
                for field in split_top_level(payload)
            ):
                continue
            if (path, match["name"], name) in EXCEPTIONS:
                continue
            line = source.count("\n", 0, start + variant_start) + 1
            results.append(f"{path}:{line}: {match['name']}::{name} is raw under derived Debug; use Secret<T>")
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
        ]:
            code = f"#[derive(Debug)] enum Fixture {{ {variant}, Other, }}"
            self.assertEqual(len(violations(code)), 1, code)

    def test_exception_is_exact(self):
        code = '#[derive(Debug)] struct ModerationMarker { token: String }'
        self.assertFalse(violations(code, "crates/core/src/mac.rs"))
        self.assertTrue(violations(code, "other.rs"))
        code = '#[derive(Debug)] enum Segment { Token(String) }'
        self.assertFalse(violations(code, "crates/core/src/voice_naming.rs"))
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
