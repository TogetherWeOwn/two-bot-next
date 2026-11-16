#!/usr/bin/env python3
"""Re-vendor the design-kit Rust tokens at the pinned tag.

Copies ``dist/rust/brand_tokens.rs`` from the pinned
TogetherWeOwn/two-design-kit release tag (recorded in
``crates/discord/src/brand.rs`` as ``BRAND_KIT_TAG``) byte-exact into
``crates/discord/src/brand_tokens.rs`` — the same curl the kit README
prescribes — then refreshes the ``BRAND_TOKENS_FNV1A64`` drift value in
``brand.rs`` so the offline ``cargo test`` drift check stays green.

Usage:
  python3 scripts/vendor_brand_tokens.py --check    # offline, read-only drift check
  python3 scripts/vendor_brand_tokens.py --check-upstream  # byte comparison, network
  python3 scripts/vendor_brand_tokens.py            # re-sync the pinned tag
  python3 scripts/vendor_brand_tokens.py --tag v0.3.0
      # bump: downloads the new tag, rewrites the tag + hash consts

Afterwards run ``cargo test -p two-bot-discord brand`` and keep the run in
the PR. Never hand-edit ``brand_tokens.rs``: the drift test fails on any
byte difference from the pinned tag's generated output.
"""

import argparse
import re
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BRAND_RS = ROOT / "crates" / "discord" / "src" / "brand.rs"
TOKENS_RS = ROOT / "crates" / "discord" / "src" / "brand_tokens.rs"

TAG_RE = re.compile(r'pub const BRAND_KIT_TAG: &str = "([^"]+)";')
HASH_RE = re.compile(r"pub const BRAND_TOKENS_FNV1A64: u64 = 0x([0-9A-Fa-f]+);")
GENERATED_MARKER = b"//! DO NOT EDIT"


def fnv1a64(data: bytes) -> int:
    digest = 0xCBF29CE484222325
    for byte in data:
        digest ^= byte
        digest = (digest * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return digest


def pinned_tag() -> str:
    match = TAG_RE.search(BRAND_RS.read_text(encoding="utf-8"))
    if not match:
        raise SystemExit("BRAND_KIT_TAG not found in crates/discord/src/brand.rs")
    return match.group(1)


def fetch(tag: str) -> bytes:
    url = (
        "https://raw.githubusercontent.com/TogetherWeOwn/two-design-kit"
        f"/{tag}/dist/rust/brand_tokens.rs"
    )
    with urllib.request.urlopen(url, timeout=60) as response:
        body = response.read()
    if not body.startswith(GENERATED_MARKER):
        raise SystemExit(f"refusing unexpected payload from {tag}: missing generator marker")
    return body


def refreshed_consts(tag: str, digest: int) -> str:
    source = BRAND_RS.read_text(encoding="utf-8")
    source, tag_count = TAG_RE.subn(f'pub const BRAND_KIT_TAG: &str = "{tag}";', source)
    replacement = f"pub const BRAND_TOKENS_FNV1A64: u64 = 0x{digest:016X};"
    source, hash_count = HASH_RE.subn(replacement, source)
    if tag_count != 1 or hash_count != 1:
        raise SystemExit("brand.rs tag/hash consts did not patch exactly once each")
    return source


def check_local() -> bool:
    source = BRAND_RS.read_text(encoding="utf-8")
    match = HASH_RE.search(source)
    if not match:
        raise SystemExit("BRAND_TOKENS_FNV1A64 not found in crates/discord/src/brand.rs")
    body = TOKENS_RS.read_bytes()
    return body.startswith(GENERATED_MARKER) and fnv1a64(body) == int(match.group(1), 16)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", default=None, help="kit release tag (default: pinned tag)")
    checks = parser.add_mutually_exclusive_group()
    checks.add_argument("--check", action="store_true", help="offline read-only drift check")
    checks.add_argument("--check-upstream", action="store_true",
                        help="compare pinned upstream bytes without writing (network)")
    args = parser.parse_args(argv)
    if args.tag and (args.check or args.check_upstream):
        parser.error("checks always use the pinned tag; --tag is only for re-vendoring")
    tag = args.tag or pinned_tag()
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        parser.error("kit tag must be a release tag such as v0.2.0")
    if args.check or args.check_upstream:
        matches = check_local()
        if args.check_upstream:
            matches = TOKENS_RS.read_bytes() == fetch(tag) and matches
        if not matches:
            print(f"brand_tokens.rs drifted from kit tag {tag}; re-vendor instead of hand-editing",
                  file=sys.stderr)
            return 1
        print(f"brand tokens match {tag} ({'upstream bytes' if args.check_upstream else 'offline digest'})")
        return 0
    body = fetch(tag)
    digest = fnv1a64(body)
    source = refreshed_consts(tag, digest)
    TOKENS_RS.write_bytes(body)
    BRAND_RS.write_text(source, encoding="utf-8")
    print(f"vendored {tag} -> {TOKENS_RS.relative_to(ROOT)} ({len(body)} bytes)")
    print(f"BRAND_KIT_TAG = {tag!r}, BRAND_TOKENS_FNV1A64 = 0x{digest:016X}")
    print("next: cargo test -p two-bot-discord brand")
    return 0


if __name__ == "__main__":
    sys.exit(main())
