#!/usr/bin/env python3
"""Read-only cutover acceptance probes for the bot HTTP surface (stdlib only).

Five checks, one PASS/FAIL line each; exit 1 if any fails, 0 if all pass.
Every check takes only GETs against {base}/health and {base}/readyz, follows
no redirect, sends no credential and writes nothing. The same invocation runs
against a local preview container now and the staging Worker later:

  python3 scripts/qa_cutover_probes.py --base-url http://127.0.0.1:8080
  python3 scripts/qa_cutover_probes.py --base-url https://two-bot-next-staging.<id>.workers.dev

Probes and the code path each one checks:

  liveness         /health answers 200 {"status":"ok"}.
                   Source: crates/bot/src/server.rs health().
  readiness-shape  /readyz answers with the bot's truthful breakdown: a
                   components array of [name, status] pairs with known
                   vocabulary, plus build_revision and build_id.
                   Source: crates/bot/src/server.rs ReadinessReport,
                   crates/core/src/health.rs HealthReport.
  gateway-state    the gateway component agrees with the HTTP status
                   (200 means ready; 503 means down/starting, i.e. parked,
                   which is not E2E approval). A 200 with a parked gateway,
                   or a 503 naming a ready gateway, fails as a lying gate.
                   Source: crates/bot/src/gateway.rs GatewayState::status,
                   crates/bot/src/server.rs readiness_after_ping.
  jobs-map         /readyz carries the informational jobs map (each entry
                   with boolean parked/running). Jobs never flip readiness.
                   Source: crates/bot/src/jobs.rs statuses,
                   crates/bot/src/server.rs failing_jobs test.
  fence-watch      the /readyz body is the container's breakdown, never a
                   Worker ownership refusal (503 {"error":"ownership_fenced"}).
                   A refusal fails: the probe did not reach the container.
                   Source: wrangler/src/ownership.ts, wrangler/src/index.ts.

A parked preview (503, gateway down, database down) is a PASS: the probe
grades truthfulness, not service readiness. Pass --expect-ready when the
cutover gate needs the service itself ready (200, gateway ready); then a
parked-but-truthful server fails gateway-state.

Usage:
  python3 scripts/qa_cutover_probes.py --base-url URL [--expect-ready]
      [--timeout SECONDS] [--evidence FILE]
"""

import argparse
import http.client
import json
import os
import sys
import time
from dataclasses import dataclass
from urllib.parse import urlsplit
import urllib.error
import urllib.request

BODY_CAP = 64 << 10
DEFAULT_TIMEOUT_SECONDS = 10
KNOWN_STATUSES = ("ready", "starting", "down")
REQUIRED_COMPONENTS = ("process", "gateway")


class ProbeError(Exception):
    """A check's one-line failure reason."""


@dataclass(frozen=True)
class Result:
    name: str
    ok: bool
    reason: str
    source: str

    def line(self):
        return f"{'PASS' if self.ok else 'FAIL'} {self.name}: {self.reason} [{self.source}]"


SRC_SERVER = "crates/bot/src/server.rs"
SRC_HEALTH = "crates/core/src/health.rs"
SRC_GATEWAY = "crates/bot/src/gateway.rs"
SRC_JOBS = "crates/bot/src/jobs.rs"
SRC_FENCE = "wrangler/src/ownership.ts"


def normalize_base(url):
    """Accept an http(s) origin without credentials; probes append their paths."""
    if not url:
        raise ProbeError("no base URL given (--base-url or QA_PROBE_BASE_URL)")
    try:
        parts = urlsplit(url)
    except ValueError:
        raise ProbeError(f"refusing: {url!r:.80} is not a usable base URL")
    try:
        port = parts.port
    except ValueError:
        raise ProbeError(f"refusing: {url!r:.80} has an unusable port")
    _ = port
    if parts.scheme not in ("http", "https"):
        raise ProbeError(f"refusing: {url!r:.80} is not http(s)")
    if parts.username or parts.password:
        raise ProbeError("refusing: base URL must not carry credentials")
    if parts.path not in ("", "/") or parts.query or parts.fragment:
        raise ProbeError(f"refusing: {url!r:.80} must be a bare origin, no path")
    host = parts.hostname or ""
    port_suffix = f":{parts.port}" if parts.port else ""
    return f"{parts.scheme}://{host}{port_suffix}"


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None  # urllib then raises the 3xx as an HTTPError


def fetch(url, timeout):
    """One GET without redirects; returns (status, body bytes)."""
    opener = urllib.request.build_opener(_NoRedirect)
    try:
        with opener.open(urllib.request.Request(url, method="GET"),
                         timeout=timeout) as response:
            return response.status, response.read(BODY_CAP + 1)
    except urllib.error.HTTPError as e:
        return e.code, e.read(BODY_CAP + 1)


def decode(body):
    try:
        return json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return None


def check_liveness(status, body):
    if status != 200:
        raise ProbeError(f"/health answered {status}, expected 200")
    if decode(body) != {"status": "ok"}:
        raise ProbeError("/health 200 without {\"status\": \"ok\"}")
    return "/health 200 {\"status\": \"ok\"}: process answers HTTP"


def check_readyz_shape(status, body):
    report = decode(body)
    if isinstance(report, dict) and report.get("error"):
        raise ProbeError(f"/readyz refused with error {report.get('error')!r:.40} "
                         "(ownership fence or proxy refusal, not the container breakdown)")
    if status in (301, 302, 303, 307, 308):
        raise ProbeError(f"/readyz answered {status}: refusing to follow a redirect")
    components = report.get("components") if isinstance(report, dict) else None
    if (not isinstance(components, list) or not components
            or not all(isinstance(c, list) and len(c) == 2
                       and isinstance(c[0], str) and c[0]
                       and c[1] in KNOWN_STATUSES for c in components)):
        raise ProbeError(f"/readyz {status} without the components breakdown "
                         "(expected [[name, ready|starting|down], ...])")
    state = dict(components)
    missing = [name for name in REQUIRED_COMPONENTS if name not in state]
    if missing:
        raise ProbeError(f"/readyz breakdown lacks {', '.join(missing)}")
    for key in ("build_revision", "build_id"):
        if not isinstance(report.get(key), str):
            raise ProbeError(f"/readyz breakdown lacks {key!r} build identity")
    ready = all(value == "ready" for value in state.values())
    expected = 200 if ready else 503
    if status != expected:
        raise ProbeError(f"/readyz {status} disagrees with its breakdown "
                         f"({'all ready' if ready else 'not all ready'}, expected {expected})")
    return state, (f"/readyz {status}: "
                   f"{', '.join(f'{k}={v}' for k, v in state.items())} "
                   f"(build {report['build_revision']:.20}/{report['build_id']:.20})")


def check_gateway(state, status, expect_ready):
    gateway = state["gateway"]
    if status == 200 and gateway == "ready":
        return "gateway ready: READY dispatch committed"
    if status == 503 and gateway in ("down", "starting"):
        note = f"gateway {gateway}: parked, not E2E approval"
        if expect_ready:
            raise ProbeError(f"{note}; --expect-ready needs gateway ready")
        return f"{note} (truthful 503, probe stays green)"
    raise ProbeError(f"gateway {gateway!r} disagrees with HTTP {status}")


def check_jobs(report):
    jobs = report.get("jobs")
    if not isinstance(jobs, dict) or not jobs:
        raise ProbeError("/readyz carries no informational jobs map")
    bad = [name for name, entry in jobs.items()
           if not isinstance(entry, dict)
           or not isinstance(entry.get("parked"), bool)
           or not isinstance(entry.get("running"), bool)]
    if bad:
        shown = ", ".join(sorted(bad)[:3]) + ("..." if len(bad) > 3 else "")
        raise ProbeError(f"/readyz jobs map has malformed entries: {shown}")
    return f"{len(jobs)} jobs reported (informational; never gates readiness)"


def run(args, fetch_fn=None):
    """Fetch once per route, then evaluate every probe on the bodies."""
    results = []

    def record(name, source, check, *check_args):
        try:
            results.append(Result(name, True, check(*check_args), source))
        except ProbeError as e:
            results.append(Result(name, False, str(e), source))

    try:
        base = normalize_base(args.base_url)
    except ProbeError as e:
        return None, [Result("base-url", False, str(e), "scripts/qa_cutover_probes.py")]

    fetch_one = fetch_fn if fetch_fn is not None else (lambda url: fetch(url, args.timeout))
    try:
        health_status, health_body = fetch_one(base + "/health")
    except (OSError, http.client.HTTPException) as e:
        return None, [Result("liveness", False,
                             f"/health did not respond ({e.__class__.__name__})", SRC_SERVER)]
    try:
        readyz_status, readyz_body = fetch_one(base + "/readyz")
    except (OSError, http.client.HTTPException) as e:
        record("liveness", SRC_SERVER, check_liveness, health_status, health_body)
        results.append(Result("readiness-shape", False,
                              f"/readyz did not respond ({e.__class__.__name__})",
                              f"{SRC_SERVER}, {SRC_HEALTH}"))
        return None, results
    # Liveness first: it is independent of the readyz body.
    record("liveness", SRC_SERVER, check_liveness, health_status, health_body)

    report = decode(readyz_body)
    try:
        state, shape_reason = check_readyz_shape(readyz_status, readyz_body)
    except ProbeError as e:
        results.append(Result("readiness-shape", False, str(e),
                              f"{SRC_SERVER}, {SRC_HEALTH}"))
        results.append(Result("gateway-state", False,
                              "no truthful breakdown to read gateway from", SRC_GATEWAY))
        results.append(Result("jobs-map", False,
                              "no truthful breakdown to read jobs from", SRC_JOBS))
        results.append(Result("fence-watch", False, str(e), SRC_FENCE)
                       if isinstance(report, dict) and report.get("error")
                       else Result("fence-watch", False,
                                   "no truthful breakdown to rule out a fence refusal",
                                   SRC_FENCE))
        return report, results
    results.append(Result("readiness-shape", True, shape_reason,
                          f"{SRC_SERVER}, {SRC_HEALTH}"))
    record("gateway-state", SRC_GATEWAY, check_gateway, state,
           readyz_status, args.expect_ready)
    record("jobs-map", SRC_JOBS, check_jobs, report)
    if isinstance(report, dict) and report.get("error"):
        results.append(Result("fence-watch", False,
                              f"container breakdown carries error {report.get('error')!r:.40}",
                              SRC_FENCE))
    else:
        results.append(Result("fence-watch", True,
                              "container breakdown, no ownership refusal", SRC_FENCE))
    return report, results


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--base-url", default=os.environ.get("QA_PROBE_BASE_URL"),
                        help="bare http(s) origin to probe (default: $QA_PROBE_BASE_URL)")
    parser.add_argument("--expect-ready", action="store_true",
                        help="fail gateway-state unless the service is ready "
                             "(default: a truthful parked 503 stays green)")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS,
                        help=f"seconds per GET (default: {DEFAULT_TIMEOUT_SECONDS})")
    parser.add_argument("--evidence", default=None,
                        help="receipt path written when both routes answer")
    return parser.parse_args(argv)


def main(argv=None, fetch_fn=None):
    args = parse_args(argv)
    report, results = run(args, fetch_fn)
    for result in results:
        print(result.line())
    failed = sum(not r.ok for r in results)
    print(f"cutover acceptance probes: {len(results) - failed}/{len(results)} checks passed")
    if report is None or not all(r.ok for r in results):
        if args.evidence and report is not None:
            pass  # Refusals and failures leave no receipt to attest.
        return 1
    if args.evidence:
        receipt = {"utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                   "result": "pass",
                   "expect_ready": args.expect_ready,
                   "checks": [{"check": r.name, "verdict": "pass",
                               "detail": r.reason, "source": r.source} for r in results]}
        try:
            with open(args.evidence, "w", encoding="utf-8") as handle:
                json.dump(receipt, handle, indent=2)
                handle.write("\n")
        except OSError as error:
            print(f"cutover acceptance probes failed: evidence not written "
                  f"({error.__class__.__name__})")
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
