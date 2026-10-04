#!/usr/bin/env python3
"""Read-only staging smoke for the go redirect surface (stdlib only).

Three GETs against the staging Worker origin, each traced to a contract line
in docs/redirect-configuration.md:

  healthz    exact `/healthz` answers 200 `ok` (probe contract: GET/HEAD only).
  reserved   a case alias of the reserved probe answers 404 with no Location
             and no lookup, throttle, diagnostic or click side effect.
  unknown    an absent valid-shape slug answers 404 with no Location, or the
             configured 302 fallback to the fixed discord.gg host.

Any 5xx on any probe must fail closed with a bounded shape: the
`redirect service misconfigured` or `temporarily unavailable` 503 with
`retry-after: 30`, or the `misconfigured campaign` 500 -- never a Location.

The smoke never writes, never follows redirects (a 302 is observed, not
followed), never touches a known campaign slug (no click side effects),
never touches production, and sends no credentials. The only network calls
are the three GETs above; any other origin, credentialed URL, or redirect
target is refused before a request is sent.

Usage:
  STAGING_WORKER_URL=https://two-bot-next-staging.<sub>.workers.dev \\
      python3 scripts/staging_redirect_smoke.py [--evidence FILE]
"""

import argparse
from dataclasses import dataclass
import http.client
import json
import os
from datetime import datetime, timezone
import re
import sys
import urllib.error
import urllib.request
from urllib.parse import urlsplit

STAGING_HOST = re.compile(r"two-bot-next-staging\.[a-z0-9-]+\.workers\.dev")
INVITE_CODE = re.compile(r"[A-Za-z0-9-]{1,64}")
VALID_SLUG = re.compile(r"[a-z0-9][a-z0-9-]{0,38}[a-z0-9]")
RESERVED_SLUGS = frozenset({"healthz", "metrics"})
DEFAULT_UNKNOWN_SLUG = "staging-smoke-unknown-slug"
BODY_CAP = 64 << 10
TIMEOUT_SECONDS = 10
FALLBACK_HOST = "https://discord.gg/"

# Fixed body shapes from docs/redirect-configuration.md and
# wrangler/src/redirect.ts. Responses are classified to one of these tokens;
# raw bodies never reach the console or the evidence file.
HEALTHZ_BODY = b"ok\n"
NOT_FOUND_BODY = b"not found\n"
MISCONFIGURED_BODY = b"redirect service misconfigured\n"
UNAVAILABLE_BODY = b"temporarily unavailable\n"
BAD_CAMPAIGN_BODY = b"misconfigured campaign\n"


class SmokeError(Exception):
    """A check's one-line failure reason (fixed vocabulary only)."""


@dataclass(frozen=True)
class Result:
    name: str
    ok: bool
    reason: str

    def line(self):
        return f"{'PASS' if self.ok else 'FAIL'} {self.name}: {self.reason}"


def staging_origin(url):
    """Allowlist the staging Worker origin; refuse everything else unread."""
    if not url:
        raise SmokeError("no staging Worker URL given (--staging-url or STAGING_WORKER_URL)")
    try:
        parts = urlsplit(url)
    except ValueError:
        raise SmokeError("refusing: not the two-bot-next-staging workers.dev origin")
    try:
        port = parts.port
    except ValueError:
        port = "invalid"
    if (parts.scheme != "https" or port is not None or parts.username or parts.password
            or not STAGING_HOST.fullmatch(parts.hostname or "")
            or parts.path not in ("", "/") or parts.query or parts.fragment):
        raise SmokeError("refusing: not the two-bot-next-staging workers.dev origin")
    return f"https://{parts.hostname}"


def probe_slug(slug):
    """The unknown-slug probe must be valid-shaped but never a real campaign.

    Reserved and malformed slugs 404 without a lookup, so they cannot
    exercise the fallback-or-404 contract; known slugs would record a click.
    """
    if not isinstance(slug, str) or not VALID_SLUG.fullmatch(slug):
        raise SmokeError("refusing: unknown-slug probe is not a valid campaign shape")
    if slug in RESERVED_SLUGS:
        raise SmokeError("refusing: unknown-slug probe must not be a reserved slug")
    return slug


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None  # urllib then raises the 3xx as an HTTPError


# The edge refuses the default `Python-urllib/*` client outright (error 1010),
# before the Worker is reached -- curl on the same probes passes. An explicit
# client UA identifies read-only smoke traffic without impersonating a browser.
USER_AGENT = "two-bot-next-staging-redirect-smoke/1.0 (read-only E2E)"


def fetch(url):
    """One GET without redirects; returns (status, headers, body bytes)."""
    opener = urllib.request.build_opener(_NoRedirect)
    request = urllib.request.Request(url, method="GET",
                                     headers={"User-Agent": USER_AGENT,
                                              "Cache-Control": "no-cache"})
    try:
        with opener.open(request, timeout=TIMEOUT_SECONDS) as response:
            return response.status, response.headers, response.read(BODY_CAP + 1)
    except urllib.error.HTTPError as error:
        return error.code, error.headers or {}, error.read(BODY_CAP + 1)


def header(headers, name):
    """Case-insensitive single header lookup; headers stay server-side."""
    if headers is None:
        return None
    get = getattr(headers, "get", None)
    if callable(get):
        try:
            return get(name)
        except (AttributeError, TypeError, ValueError):
            return None
    if isinstance(headers, dict):
        for key, value in headers.items():
            if isinstance(key, str) and key.lower() == name:
                return value if isinstance(value, str) else None
    return None


def fail_closed_shape(status, headers, body):
    """Classify a 5xx as a bounded fail-closed shape, or refuse it.

    Returns the shape token on success; raises SmokeError otherwise.
    """
    location = header(headers, "location")
    if location is not None:
        raise SmokeError(f"unexpected status {status} with a redirect target")
    if status == 503 and header(headers, "retry-after") == "30":
        if body == MISCONFIGURED_BODY:
            return "503 redirect service misconfigured with retry-after 30"
        if body == UNAVAILABLE_BODY:
            return "503 temporarily unavailable with retry-after 30"
    if status == 500 and body == BAD_CAMPAIGN_BODY:
        return "500 misconfigured campaign without a redirect target"
    raise SmokeError(f"unexpected status {status} outside the bounded fail-closed shapes")


def check_healthz(origin, fetch_fn):
    try:
        status, headers, body = fetch_fn(origin + "/healthz")
    except (OSError, http.client.HTTPException) as error:
        raise SmokeError(f"healthz probe did not respond ({error.__class__.__name__})")
    if status != 200:
        if 500 <= status <= 599:
            return fail_closed_shape(status, headers, body)
        raise SmokeError(f"healthz answered {status}, expected the exact 200 probe")
    if body != HEALTHZ_BODY or header(headers, "location") is not None:
        raise SmokeError("healthz 200 without the exact probe shape")
    return "healthz 200 with the exact probe shape"


def check_reserved(origin, fetch_fn):
    # A case alias: reserved before configuration, lookup, throttle and clicks.
    # The contract pins 404 with no redirect target, not the byte shape: the
    # Worker-level guard answers `not found`, while redirect.ts answers
    # `not found\n`. Both are the documented 404-with-no-lookup.
    try:
        status, headers, body = fetch_fn(origin + "/HEALTHZ")
    except (OSError, http.client.HTTPException) as error:
        raise SmokeError(f"reserved-alias probe did not respond ({error.__class__.__name__})")
    if status != 404:
        if 500 <= status <= 599:
            return fail_closed_shape(status, headers, body)
        raise SmokeError(f"reserved alias answered {status}, expected 404 with no lookup")
    if header(headers, "location") is not None:
        raise SmokeError("reserved alias 404 carries a redirect target")
    return "reserved alias 404 with no redirect target"


def check_unknown(origin, slug, fetch_fn):
    try:
        status, headers, body = fetch_fn(origin + "/" + slug)
    except (OSError, http.client.HTTPException) as error:
        raise SmokeError(f"unknown-slug probe did not respond ({error.__class__.__name__})")
    location = header(headers, "location")
    if status == 404:
        if body != NOT_FOUND_BODY or location is not None:
            raise SmokeError("unknown slug 404 with an unexpected shape")
        return "unknown slug 404 with no redirect target"
    if status == 302:
        if not isinstance(location, str) or not location.startswith(FALLBACK_HOST):
            raise SmokeError("unknown slug fallback leaves the fixed redirect host")
        if not INVITE_CODE.fullmatch(location[len(FALLBACK_HOST):]):
            raise SmokeError("unknown slug fallback carries an invalid invite code")
        cache = header(headers, "cache-control") or ""
        if "no-store" not in cache.lower():
            raise SmokeError("unknown slug fallback without no-store caching")
        return "unknown slug follows the configured fallback to the fixed host"
    if 500 <= status <= 599:
        return fail_closed_shape(status, headers, body)
    raise SmokeError(f"unknown slug answered {status}, expected fallback-or-404")


def run(args, fetch_fn=fetch):
    results = []
    try:
        origin = staging_origin(args.staging_url)
        slug = probe_slug(args.unknown_slug)
    except SmokeError as error:
        return None, [Result("origin", False, str(error))]
    checks = [("healthz", check_healthz, (origin,)),
              ("reserved", check_reserved, (origin,)),
              ("unknown", check_unknown, (origin, slug))]
    for name, check, check_args in checks:
        try:
            results.append(Result(name, True, check(*check_args, fetch_fn)))
        except SmokeError as error:
            results.append(Result(name, False, str(error)))
    return origin, results


def evidence_shape(results):
    """Allowlisted receipt: check names, verdicts and shape tokens only."""
    return [{"check": result.name, "verdict": "pass" if result.ok else "fail",
             "shape": result.reason} for result in results]


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--staging-url",
                        default=os.environ.get("STAGING_WORKER_URL")
                        or os.environ.get("STAGING_URL"),
                        help="staging Worker origin (default: $STAGING_WORKER_URL)")
    parser.add_argument("--unknown-slug", default=DEFAULT_UNKNOWN_SLUG,
                        help="absent valid-shape slug for the fallback-or-404 probe")
    parser.add_argument("--evidence", default="staging-redirect-smoke-evidence.json",
                        help="receipt path written only when every check passes")
    return parser.parse_args(argv)


def main(argv=None, fetch_fn=fetch):
    args = parse_args(argv)
    origin, results = run(args, fetch_fn)
    for result in results:
        print(result.line())
    failed = sum(not result.ok for result in results)
    print(f"staging redirect smoke: {len(results) - failed}/{len(results)} checks passed")
    if failed or origin is None:
        return 1
    receipt = {"origin": origin,
               "utc": datetime.now(timezone.utc).isoformat(timespec="seconds"),
               "unknown_slug": args.unknown_slug,
               "checks": evidence_shape(results),
               "result": "pass"}
    try:
        with open(args.evidence, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=2)
            handle.write("\n")
    except OSError as error:
        print(f"staging redirect smoke failed: evidence not written ({error.__class__.__name__})")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
