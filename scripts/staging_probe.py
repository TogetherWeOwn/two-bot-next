#!/usr/bin/env python3
"""Read-only staging probe for the two-bot-next Worker (TOG-12197).

B4 (TOG-9699) needs staging E2E evidence on the tested revision before any
production deploy. The deploy gate only checks /health and /readyz; this probe
also exercises the public go.two.gg surface the Worker serves (reserved paths,
fallback, ops metrics) and emits a JSON report with PASS/FAIL per probe.

Read-only by construction: GET/HEAD only, no credentials, no request bodies,
redirects are never followed, and unknown-slug checks use HEAD (never counted
as a click, even if the slug existed).

Host fence: the target comes only from --base-url and must be a bare https
origin on STAGING_HOSTS. Production hosts (go.two.gg, the production Worker)
are refused before any request. Widening the allowlist is a reviewed code
change, never a flag.

    python3 scripts/staging_probe.py \\
        --base-url https://two-bot-next-staging.5150.workers.dev \\
        --worker-version <Current Version ID from the deploy-staging run> \\
        --report staging-probe.json

Exit status: 0 PASS, 1 FAIL, 2 refused (fence or invalid arguments).

Body-cap refusal contract: the transport reads at most MAX_BODY_BYTES+1
bytes so a hidden suffix is detected. Any body longer than MAX_BODY_BYTES
(64 KiB) is refused before decoding/parsing with the fixed message
"<METHOD> <path>: answered over the body cap"; exact-cap valid input passes,
and the extra byte never reaches a successful JSON path. Refusals never echo
the body, headers, URL or parse exceptions.
"""

from __future__ import annotations

import argparse
import http.client
import json
import re
import secrets
import sys
import urllib.parse
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable

# Exact hosts only. The staging Worker is `two-bot-next-staging` (wrangler
# `--env staging`) on the account's workers.dev subdomain.
STAGING_HOSTS = frozenset({"two-bot-next-staging.5150.workers.dev"})
# Refused even if someone adds them to STAGING_HOSTS: the redirect domain and
# the Worker names wrangler deploys for the default and `--env production`
# environments, on any workers.dev subdomain.
PRODUCTION_DOMAIN = "two.gg"
PRODUCTION_WORKER_NAMES = frozenset({"two-bot-next", "two-bot-next-production"})

USER_AGENT = "two-bot-next-staging-probe/1 (TOG-12197; read-only)"
MAX_BODY_BYTES = 64 * 1024

VERSION_ID = re.compile(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}")
# Mirrors wrangler/src/redirect.ts INVITE_CODE.
INVITE_CODE = re.compile(r"[A-Za-z0-9-]{1,64}")
INVITE_URL = re.compile(r"https://discord\.gg/([A-Za-z0-9-]{1,64})")
COMPONENT_STATES = frozenset({"ready", "starting", "down"})
REQUIRED_COMPONENTS = ("process", "gateway")

# Aliases and subpaths of the reserved internal slugs (redirect.ts
# RESERVED_SLUGS). Each must 404 with no Location: never a campaign lookup,
# never a redirect, never a click.
HEALTHZ_ALIASES = ("/healthz/", "//healthz", "/HEALTHZ", "/%68ealthz", "/%2fhealthz", "/healthz/x")
METRICS_ALIASES = ("/metrics", "/metrics/", "//metrics", "/Metrics", "/%6detrics", "/metrics/x")
OPS_METRICS_PATH = "/ops/metrics"

# Internal error text that must never reach a public response. Reports name the
# pattern only, never the matched text (it could carry config or secrets).
INTERNAL_ERROR_PATTERNS = {
    "js_error": re.compile(r"\b(?:Type|Reference|Syntax|Range)Error\b"),
    "stack_frame": re.compile(r"\bat \S+ \(|\.(?:ts|js|mjs|rs):\d+"),
    "rust_panic": re.compile(r"panicked at|stack backtrace"),
    "python_traceback": re.compile(r"Traceback \(most recent call last\)"),
    "worker_exception": re.compile(r"Worker threw exception|error code: 1\d{3}\b"),
    "connection_string": re.compile(r"postgres(?:ql)?://", re.IGNORECASE),
    "secret_name": re.compile(r"DISCORD_TOKEN|DATABASE_URL|METRICS_SCRAPE_TOKEN|OPS_ALERT_WEBHOOK_URL"),
    "bearer_token": re.compile(r"Bearer \S"),
}


class FenceError(ValueError):
    """The base URL is not an allowed staging target."""


def is_production_host(host: str) -> bool:
    label = host.split(".", 1)[0]
    return (
        host == PRODUCTION_DOMAIN
        or host.endswith("." + PRODUCTION_DOMAIN)
        or label in PRODUCTION_WORKER_NAMES
        or "prod" in label
    )


def fence(base_url: str) -> str:
    """Return the canonical staging origin, or raise FenceError before any I/O."""
    try:
        parts = urllib.parse.urlsplit(base_url)
        port = parts.port
    except ValueError as exc:
        raise FenceError("base URL does not parse") from exc
    if parts.scheme != "https":
        raise FenceError("base URL must use https")
    if parts.username is not None or parts.password is not None:
        raise FenceError("base URL must not carry credentials")
    if port not in (None, 443):
        raise FenceError("base URL must use the default https port")
    if parts.path not in ("", "/") or parts.query or parts.fragment:
        raise FenceError("base URL must be a bare origin")
    host = (parts.hostname or "").rstrip(".")
    if is_production_host(host):
        raise FenceError(f"refusing production host {host}")
    if host not in STAGING_HOSTS:
        raise FenceError(f"host {host or '(none)'} is not on the staging allowlist")
    return f"https://{host}"


@dataclass(frozen=True)
class Reply:
    status: int
    headers: dict[str, str]
    body: str


class BodyTooLarge(ValueError):
    """A response body exceeds MAX_BODY_BYTES (fixed vocabulary only)."""


Fetch = Callable[[str, str], Reply]


def make_fetch(origin: str, timeout: float = 20.0) -> Fetch:
    """Bind a GET/HEAD transport to one origin. http.client never follows redirects.

    main() only passes fenced https origins; the http branch serves the local
    test fixture.
    """
    parts = urllib.parse.urlsplit(origin)
    connection = http.client.HTTPSConnection if parts.scheme == "https" else http.client.HTTPConnection

    def fetch(method: str, path: str) -> Reply:
        if method not in ("GET", "HEAD") or not path.startswith("/"):
            raise ValueError("read-only probe: GET/HEAD on an absolute path only")
        conn = connection(parts.hostname, parts.port, timeout=timeout)
        try:
            conn.request(method, path, headers={"user-agent": USER_AGENT, "accept": "*/*"})
            response = conn.getresponse()
            raw = response.read(MAX_BODY_BYTES + 1)
            if len(raw) > MAX_BODY_BYTES:
                raise BodyTooLarge("response body over the size cap")
            headers = {name.lower(): value for name, value in response.getheaders()}
            return Reply(response.status, headers, raw.decode("utf-8", "replace"))
        finally:
            conn.close()

    return fetch


class Probe:
    """One named check: its requests, failures and a small observed summary."""

    def __init__(self, name: str, fetch: Fetch, seen: list[tuple[str, str, Reply]]):
        self.name = name
        self._fetch = fetch
        self._seen = seen
        self.requests: list[dict[str, object]] = []
        self.failures: list[str] = []
        self.observed: dict[str, object] = {}

    def request(self, method: str, path: str) -> Reply | None:
        try:
            reply = self._fetch(method, path)
        except BodyTooLarge:
            # Fixed refusal: no body, headers, URL or exception text echoed.
            self.requests.append({"method": method, "path": path, "status": None,
                                  "error": "BodyTooLarge"})
            self.fail(f"{method} {path}: answered over the body cap")
            return None
        except (OSError, http.client.HTTPException) as exc:
            # Type only: exception text can echo hosts, headers or bodies.
            self.requests.append({"method": method, "path": path, "status": None, "error": type(exc).__name__})
            self.fail(f"{method} {path}: no response ({type(exc).__name__})")
            return None
        if len(reply.body.encode("utf-8")) > MAX_BODY_BYTES:
            # Injected-transport oversize (test doubles bypass make_fetch):
            # refuse before parsing, without echoing body/headers or recording
            # a Location value, and without adding to the error-text scan.
            self.requests.append({"method": method, "path": path, "status": reply.status})
            self.fail(f"{method} {path}: answered over the body cap")
            return None
        entry: dict[str, object] = {"method": method, "path": path, "status": reply.status}
        if "location" in reply.headers:
            entry["location"] = reply.headers["location"]
        self.requests.append(entry)
        self._seen.append((method, path, reply))
        return reply

    def fail(self, message: str) -> None:
        self.failures.append(message)

    def expect(self, ok: bool, message: str) -> None:
        if not ok:
            self.fail(message)

    def expect_not_found(self, method: str, path: str) -> None:
        reply = self.request(method, path)
        if reply is None:
            return
        self.expect(reply.status == 404, f"{method} {path}: expected 404, got {reply.status}")
        self.expect("location" not in reply.headers, f"{method} {path}: must not carry a Location header")

    def result(self) -> dict[str, object]:
        out: dict[str, object] = {"name": self.name, "verdict": "FAIL" if self.failures else "PASS"}
        if self.observed:
            out["observed"] = self.observed
        out["failures"] = self.failures
        out["requests"] = self.requests
        return out


def parse_json(reply: Reply) -> object:
    try:
        return json.loads(reply.body)
    except ValueError:
        return None


def probe_health(p: Probe) -> None:
    reply = p.request("GET", "/health")
    if reply is None:
        return
    p.expect(reply.status == 200, f"GET /health: expected 200, got {reply.status}")
    body = parse_json(reply)
    p.expect(isinstance(body, dict) and body.get("status") == "ok", 'GET /health: body is not {"status":"ok"}')


def probe_readyz(p: Probe, require_ready: bool) -> None:
    """Truthful: 200 iff every component is ready, 503 otherwise, always JSON."""
    reply = p.request("GET", "/readyz")
    if reply is None:
        return
    p.expect(reply.status in (200, 503), f"GET /readyz: expected 200 or 503, got {reply.status}")
    body = parse_json(reply)
    components = body.get("components") if isinstance(body, dict) else None
    well_formed = (
        isinstance(components, list)
        and len(components) > 0
        and all(
            isinstance(c, list) and len(c) == 2 and isinstance(c[0], str) and c[1] in COMPONENT_STATES
            for c in components
        )
    )
    if not well_formed:
        p.fail("GET /readyz: body is not a JSON component breakdown")
        return
    # Reject duplicates before any readiness math: last-wins would let a
    # later ready mask an earlier down.
    names = [name for name, _ in components]
    if len(set(names)) != len(names):
        p.fail("GET /readyz: repeats a component name")
        return
    states = {name: state for name, state in components}
    ready = all(state == "ready" for state in states.values())
    p.observed = {"status": reply.status, "ready": ready, "components": states}
    for name in REQUIRED_COMPONENTS:
        p.expect(name in states, f"GET /readyz: missing component {name}")
    if reply.status in (200, 503):
        p.expect((reply.status == 200) == ready, f"GET /readyz: status {reply.status} contradicts components")
    if require_ready:
        p.expect(ready, "GET /readyz: not ready (--require-ready)")


def probe_reserved_healthz(p: Probe) -> None:
    for method in ("GET", "HEAD"):
        reply = p.request(method, "/healthz")
        if reply is None:
            continue
        p.expect(reply.status == 200, f"{method} /healthz: expected 200, got {reply.status}")
        p.expect("location" not in reply.headers, f"{method} /healthz: must not redirect")
        if method == "GET":
            p.expect(reply.body == "ok\n", 'GET /healthz: body is not "ok\\n"')
    for path in HEALTHZ_ALIASES:
        p.expect_not_found("GET", path)


def probe_fallback(p: Probe, unknown_slug: str, expect_code: str | None) -> None:
    """`/` and an unknown slug: 302 only to the configured invite, else 404.

    The Worker serves the configured fallback for `/` and for lookup outages; an
    unknown slug with a healthy store is 404 with no Location (no open redirect).
    """
    root = p.request("HEAD", "/")
    unknown_path = f"/{unknown_slug}"
    unknown = p.request("HEAD", unknown_path)
    fallback: str | None = None
    if root is not None:
        location = root.headers.get("location")
        match = INVITE_URL.fullmatch(location or "")
        if root.status == 302 and match:
            fallback = location
            p.observed["fallback"] = "configured"
            p.observed["fallback_location"] = location
            p.expect(
                "no-store" in root.headers.get("cache-control", ""),
                "HEAD /: fallback redirect must be cache-control no-store",
            )
            if expect_code is not None:
                p.expect(match.group(1) == expect_code, f"HEAD /: fallback is not discord.gg/{expect_code}")
        elif root.status == 404 and location is None:
            p.observed["fallback"] = "unset"
            p.expect(expect_code is None, f"HEAD /: expected fallback discord.gg/{expect_code}, got 404")
        else:
            p.fail(f"HEAD /: expected 302 to a discord.gg invite or 404, got {root.status}")
    if unknown is not None:
        location = unknown.headers.get("location")
        if unknown.status == 404 and location is None:
            p.observed["unknown_slug"] = "not_found"
        elif unknown.status == 302 and fallback is not None and location == fallback:
            p.observed["unknown_slug"] = "fallback"
        else:
            p.fail(
                f"HEAD {unknown_path}: expected 404 or 302 to the configured fallback, "
                f"got {unknown.status}" + (f" to {location}" if location else "")
            )


def probe_ops_metrics(p: Probe) -> None:
    reply = p.request("GET", OPS_METRICS_PATH)
    if reply is not None:
        p.expect(
            reply.status in (401, 404),
            f"GET {OPS_METRICS_PATH} without a bearer: expected 401 or 404, got {reply.status}",
        )
        p.expect("# TYPE " not in reply.body and "# HELP " not in reply.body,
                 f"GET {OPS_METRICS_PATH} without a bearer: body carries metrics exposition")
        p.observed["ops_metrics_status"] = reply.status
    for path in METRICS_ALIASES:
        p.expect_not_found("GET", path)


def probe_error_text(p: Probe, seen: list[tuple[str, str, Reply]]) -> None:
    """No internal error text anywhere; no 5xx except the truthful /readyz 503."""
    p.observed["responses_checked"] = len(seen)
    for method, path, reply in seen:
        for name, pattern in INTERNAL_ERROR_PATTERNS.items():
            if pattern.search(reply.body):
                p.fail(f"{method} {path}: body matches internal error pattern {name}")
        if reply.status >= 500 and not (path == "/readyz" and reply.status == 503):
            p.fail(f"{method} {path}: server error {reply.status}")


def run_probes(
    fetch: Fetch,
    *,
    unknown_slug: str | None = None,
    expect_fallback_code: str | None = None,
    require_ready: bool = False,
) -> list[dict[str, object]]:
    seen: list[tuple[str, str, Reply]] = []
    slug = unknown_slug or f"probe-{secrets.token_hex(6)}"
    steps: list[tuple[str, Callable[[Probe], None]]] = [
        ("health", probe_health),
        ("readyz", lambda p: probe_readyz(p, require_ready)),
        ("reserved_healthz", probe_reserved_healthz),
        ("fallback", lambda p: probe_fallback(p, slug, expect_fallback_code)),
        ("ops_metrics", probe_ops_metrics),
        ("no_internal_error_text", lambda p: probe_error_text(p, seen)),
    ]
    results = []
    for name, step in steps:
        probe = Probe(name, fetch, seen)
        step(probe)
        results.append(probe.result())
    return results


def utc_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def build_report(
    origin: str,
    worker_version: str,
    fetch: Fetch,
    *,
    unknown_slug: str | None = None,
    expect_fallback_code: str | None = None,
    require_ready: bool = False,
) -> dict[str, Any]:
    start = utc_now()
    probes = run_probes(
        fetch,
        unknown_slug=unknown_slug,
        expect_fallback_code=expect_fallback_code,
        require_ready=require_ready,
    )
    end = utc_now()
    return {
        "probe": "two-bot-next staging Worker probe (TOG-12197)",
        "schema": 1,
        "target": origin,
        "worker_version": worker_version,
        "window_utc": {"start": start, "end": end},
        "verdict": "PASS" if all(p["verdict"] == "PASS" for p in probes) else "FAIL",
        "probes": probes,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--base-url", required=True, help="staging Worker origin (https, allowlisted)")
    parser.add_argument(
        "--worker-version",
        required=True,
        help='Worker version under test: the "Current Version ID" the deploy-staging run printed',
    )
    parser.add_argument("--expect-fallback-code", help="invite code `/` must redirect to (default: observe)")
    parser.add_argument("--require-ready", action="store_true", help="FAIL unless /readyz is 200")
    parser.add_argument("--report", type=Path, help="write the JSON report here (default: stdout)")
    parser.add_argument("--timeout", type=float, default=20.0, help="per-request timeout in seconds")
    args = parser.parse_args(argv)

    try:
        origin = fence(args.base_url)
    except FenceError as exc:
        print(f"staging_probe: refused: {exc}", file=sys.stderr)
        return 2
    if not VERSION_ID.fullmatch(args.worker_version):
        print("staging_probe: --worker-version must be a Worker version ID (UUID)", file=sys.stderr)
        return 2
    if args.expect_fallback_code is not None and not INVITE_CODE.fullmatch(args.expect_fallback_code):
        print("staging_probe: --expect-fallback-code is not a valid invite code", file=sys.stderr)
        return 2

    report = build_report(
        origin,
        args.worker_version,
        make_fetch(origin, args.timeout),
        expect_fallback_code=args.expect_fallback_code,
        require_ready=args.require_ready,
    )
    text = json.dumps(report, indent=2) + "\n"
    if args.report:
        args.report.write_text(text)
    else:
        sys.stdout.write(text)
    for probe in report["probes"]:
        print(f"{probe['verdict']:4}  {probe['name']}", file=sys.stderr)
    print(f"staging_probe: {report['verdict']} {origin} version {args.worker_version}", file=sys.stderr)
    return 0 if report["verdict"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
