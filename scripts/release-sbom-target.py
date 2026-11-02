"""Bind release inventories to the peeled tag commit; dry-runs never publish."""

import argparse
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys


SHAPE_REASON = "Release tag response has invalid shape"
LOOKUP_REASON = "Release tag lookup failed"
OVER_CAP = "Release tag response exceeds the byte cap"
DUP_KEYS = "Release tag response carries duplicate object keys"
BAD_CONST = "Release tag response carries a non-finite number"
TOO_DEEP = "Release tag response is too deeply nested"
SOURCE_OVER_CAP = "SBOM source file exceeds the byte cap"
SOURCE_MISSING = "SBOM source file is missing or unreadable"
# Real gh responses are under 1 KiB; the 2 MiB fixture must refuse.
RESPONSE_CAP = 1 << 20
# Real ref responses nest two levels deep.
MAX_DEPTH = 32

# Strings are skipped whole, so brackets inside them never count. An
# unterminated string runs to the end, which keeps the scan linear.
JSON_TOKEN = re.compile(r'"(?:[^"\\]|\\.)*(?:"|\\?\Z)|[\[\]{}]', re.DOTALL)


def _json_depth_exceeds(text, limit):
    """Lexical nesting pre-scan, run before json.loads.

    The decoder's own RecursionError is no longer a reliable bound: newer
    CPython sizes it from the real C stack and decodes 10,000 levels.
    """
    depth = 0
    for match in JSON_TOKEN.finditer(text):
        token = match.group()
        if token in "[{":
            depth += 1
            if depth > limit:
                return True
        elif token in "]}":
            depth -= 1
    return False


def require_env(name):
    value = os.environ.get(name)
    if value is None:
        raise ValueError(f"{name} is not set")
    return value


def _reject_duplicate_keys(pairs):
    seen = set()
    for key, _ in pairs:
        if key in seen:
            raise ValueError(DUP_KEYS)
        seen.add(key)
    return dict(pairs)


def _reject_constant(value):
    raise ValueError(BAD_CONST)


def _reject_non_finite_float(text):
    number = float(text)
    if not math.isfinite(number):
        raise ValueError(BAD_CONST)
    return number


def parse_response(raw):
    if len(raw.encode("utf-8")) > RESPONSE_CAP:
        raise ValueError(OVER_CAP)
    if _json_depth_exceeds(raw, MAX_DEPTH):
        raise ValueError(TOO_DEEP)
    try:
        return json.loads(
            raw,
            object_pairs_hook=_reject_duplicate_keys,
            parse_constant=_reject_constant,
            parse_float=_reject_non_finite_float,
        )
    except RecursionError:
        raise ValueError(TOO_DEEP) from None


def github_object(path):
    """Fetch one git ref object; refuse transport, intake and shape failures with fixed vocabulary.

    gh stderr is discarded so service diagnostics never precede the fixed
    refusal, and undecodable stdout refuses as invalid shape. The reply is
    parsed with the intake guard (byte cap, duplicate keys, non-finite
    numbers, nesting depth), whose refusals propagate; any other parse
    failure (malformed JSON, over-limit integer tokens) refuses as
    invalid shape, never raw parser text.
    Returns the full ``gh api`` response dict; the ``object`` member is
    guaranteed present and a dict. Callers index ``["object"]`` as before,
    so the pre-existing mock contract (outer shape) is preserved.
    """
    repo = os.environ.get("GH_REPO")
    if repo is None:
        raise ValueError("GH_REPO is not set")
    try:
        raw = subprocess.check_output(
            ["gh", "api", f"repos/{repo}/git/{path}"],
            text=True, timeout=30, stderr=subprocess.DEVNULL,
        )
    except UnicodeDecodeError:
        raise ValueError(SHAPE_REASON) from None
    except (subprocess.SubprocessError, OSError):
        raise ValueError(LOOKUP_REASON) from None
    try:
        response = parse_response(raw)
    except ValueError as error:
        if str(error) in (OVER_CAP, DUP_KEYS, BAD_CONST, TOO_DEEP):
            raise
        raise ValueError(SHAPE_REASON) from None
    if not isinstance(response, dict) or not isinstance(response.get("object"), dict):
        raise ValueError(SHAPE_REASON)
    return response


def resolve_tag(tag):
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError("Publication requires a stable vX.Y.Z release tag")
    # The Git refs API names only tags: a same-named branch cannot win checkout.
    # github_object() guarantees the outer shape, so this index cannot KeyError.
    obj = github_object(f"ref/tags/{tag}")["object"]
    for _ in range(8):
        sha = obj.get("sha", "")
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
            raise ValueError("Invalid release tag object SHA")
        if obj.get("type") == "commit":
            return obj["sha"]
        if obj.get("type") != "tag":
            raise ValueError("Release tag does not resolve to a commit")
        obj = github_object(f"tags/{obj['sha']}")["object"]
    raise ValueError("Release tag nesting exceeds the resolution bound")


def select_target(dry_run, tag, sha):
    if dry_run == "true":
        if not re.fullmatch(r"[0-9a-f]{40}", sha):
            raise ValueError("Dry-run requires the workflow source SHA")
        return {"ref": sha, "tag": ""}
    return {"ref": resolve_tag(tag), "tag": tag}


def read_capped_source(path):
    try:
        if path.stat().st_size > RESPONSE_CAP:
            raise ValueError(SOURCE_OVER_CAP)
        return path.read_text()
    except OSError:
        raise ValueError(SOURCE_MISSING) from None


def verify_source(tag, expected_sha, source):
    if not re.fullmatch(r"[0-9a-f]{40}", expected_sha) or read_capped_source(source).strip() != expected_sha:
        raise ValueError("SBOM source differs from the selected release commit")
    if resolve_tag(tag) != expected_sha:
        raise ValueError("Release tag moved since SBOM source selection")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-source", type=Path)
    args = parser.parse_args()
    try:
        if args.verify_source:
            verify_source(require_env("RELEASE_TAG"), require_env("EXPECTED_SHA"),
                            args.verify_source)
        else:
            dry_run = os.environ.get("DRY_RUN", "")
            sha = require_env("GITHUB_SHA")
            output_path = require_env("GITHUB_OUTPUT")
            target = select_target(dry_run, os.environ.get("RELEASE_TAG", ""), sha)
            with open(output_path, "a") as output:
                for name, value in target.items():
                    output.write(f"{name}={value}\n")
    except ValueError as error:
        print(error, file=sys.stderr)
        raise SystemExit(1)
