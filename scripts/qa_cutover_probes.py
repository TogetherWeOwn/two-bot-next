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
  gateway-state    the gateway component is truthful (ready means READY
                   dispatch committed; down/starting means parked, which is
                   not E2E approval). readiness-shape already proves the HTTP
                   status matches the breakdown, so gateway-ready + 503 (e.g.
                   database down) passes: only the service is not ready.
                   Pass --expect-ready when the gate needs readiness itself.
                   Source: crates/bot/src/gateway.rs GatewayState::status,
                   crates/bot/src/server.rs readiness_after_ping.
  jobs-map         /readyz carries the informational jobs map (each entry
                   with boolean parked/running). Jobs never flip readiness,
                   but when the service reports ready (200) or --expect-ready
                   is set, every unparked job with a known code-constant
                   cadence must show a recent last_success: a job older than
                   its max_last_success_age_seconds (2x cadence + timeout)
                   fails and is named; a job with no success but failed
                   attempts (consecutive_failures > 0, or a last_start older
                   than the fail line) fails and is named. While degraded
                   without --expect-ready the jobs map stays informational so
                   a truthful parked 503 stays green. feeds is ungraded (its
                   cadence is env-configured), as is any unknown job name.
                   Source: crates/bot/src/jobs.rs statuses,
                   crates/bot/src/server.rs failing_jobs test.
  fence-watch      the /readyz body is the container's breakdown, never a
                   Worker ownership refusal (503 {"error":"ownership_fenced"}).
                   A refusal fails: the probe did not reach the container.
                   Source: wrangler/src/ownership.ts, wrangler/src/index.ts.

A parked preview (503, gateway down, database down) is a PASS: the probe
grades truthfulness, not service readiness, and the jobs map is not graded
for freshness while degraded. Pass --expect-ready when the cutover gate
needs the service itself ready (200, gateway ready); then a
parked-but-truthful server fails gateway-state and stale/failing jobs fail
jobs-map.

Body-cap refusal contract: the transport reads at most BODY_CAP+1 bytes on
both the normal and HTTPError paths so a hidden suffix is detected. Any body
longer than BODY_CAP (64 KiB) is refused before JSON parsing with the fixed
message "/health answered over the body cap" or "/readyz answered over the
body cap"; exact-cap valid input passes, and the extra byte never reaches a
successful JSON path. Refusals never echo the body, headers, URL or parse
exceptions.

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
# Cloudflare rejects the default "Python-urllib/x.y" agent at the edge with 403
# (error 1010), so an explicit agent is required for the probe to reach the Worker.
USER_AGENT = "two-bot-next-staging-rollout/1.0"
KNOWN_STATUSES = ("ready", "starting", "down")
REQUIRED_COMPONENTS = ("process", "gateway")
# Per-job last_success freshness bands, in seconds, derived from the jobs'
# real cadences plus their per-attempt timeouts (a healthy sample's age runs
# up to about one cadence plus runtime; last_success only advances on success,
# crates/bot/src/jobs.rs record_completion). Warn is ~1x cadence + timeout
# (one missed cycle is suspicious); max is ~2x cadence + timeout (two missed
# cycles proves stuck, not jittered):
#   counter: 60s cadence (LIVE_COUNTER_INTERVAL_MS, community_snapshots.rs:45)
#     + 45s timeout (website_jobs.rs) -> warn 105, max 165
#   member_unban_sweep: 30s cadence (UNBAN_SWEEP_INTERVAL_SECONDS,
#     member_moderation.rs:69) + 180s timeout (SWEEP_TIMEOUT,
#     member_runtime.rs:57) -> warn 210, max 240
#   scheduled_messages: 15s cadence (SCHEDULER_TICK_MS, scheduled.rs:49)
#     + 120s timeout (scheduled_jobs.rs) -> warn 135, max 150
#   settings: 15s cadence (POLL_SECONDS, settings.rs:47) + 10s timeout
#     (TIMEOUT, settings_jobs.rs:41) -> warn 25, max 40
#   rank / scheduled_events: 600s cadence (RANK_SNAPSHOT_INTERVAL_MS,
#     community_snapshots.rs:47; SCHEDULED_EVENTS_INTERVAL_MS,
#     scheduled_events.rs:26) + 120s timeout (website_jobs.rs) ->
#     warn 720, max 1320
#   inactivity / presence_probe: 3600s cadence (INACTIVITY_SWEEP_INTERVAL_MS,
#     inactivity.rs:27; PRESENCE_PROBE_INTERVAL_MS, presence.rs:36) + 120s
#     timeout (community_jobs.rs) -> warn 3720, max 7320
# feeds is deliberately ungraded: its cadence is env-configured
# (TWO_FEED_POLL_SECONDS, default 300, validated 60-86400,
# feature_commands.rs:337-359; 120s JOB_TIMEOUT, feed_jobs.rs:41), so the
# probe cannot know the configured value and must not grade it.
JOB_CADENCE_SECONDS = {
    "counter": 60,
    "member_unban_sweep": 30,
    "scheduled_messages": 15,
    "settings": 15,
    "rank": 600,
    "scheduled_events": 600,
    "inactivity": 3600,
    "presence_probe": 3600,
}
JOB_TIMEOUT_SECONDS = {
    "counter": 45,
    "member_unban_sweep": 180,
    "scheduled_messages": 120,
    "settings": 10,
    "rank": 120,
    "scheduled_events": 120,
    "inactivity": 120,
    "presence_probe": 120,
}
WARN_LAST_SUCCESS_AGE_SECONDS = {
    name: JOB_CADENCE_SECONDS[name] + JOB_TIMEOUT_SECONDS[name]
    for name in JOB_CADENCE_SECONDS
}
MAX_LAST_SUCCESS_AGE_SECONDS = {
    name: 2 * JOB_CADENCE_SECONDS[name] + JOB_TIMEOUT_SECONDS[name]
    for name in JOB_CADENCE_SECONDS
}


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
        parts.port
    except ValueError:
        raise ProbeError(f"refusing: {url!r:.80} has an unusable port")
    if parts.scheme not in ("http", "https"):
        raise ProbeError(f"refusing: {url!r:.80} is not http(s)")
    if parts.username or parts.password:
        raise ProbeError("refusing: base URL must not carry credentials")
    if parts.path not in ("", "/") or parts.query or parts.fragment:
        raise ProbeError(f"refusing: {url!r:.80} must be a bare origin, no path")
    host = parts.hostname or ""
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    port_suffix = f":{parts.port}" if parts.port else ""
    return f"{parts.scheme}://{host}{port_suffix}"


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None  # urllib then raises the 3xx as an HTTPError


def fetch(url, timeout):
    """One GET without redirects; returns (status, body bytes)."""
    opener = urllib.request.build_opener(_NoRedirect)
    try:
        with opener.open(urllib.request.Request(url, headers={"User-Agent": USER_AGENT},
                                               method="GET"),
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
    if len(body) > BODY_CAP:
        raise ProbeError("/health answered over the body cap")
    if decode(body) != {"status": "ok"}:
        raise ProbeError("/health 200 without {\"status\": \"ok\"}")
    return "/health 200 {\"status\": \"ok\"}: process answers HTTP"


def check_readyz_shape(status, body):
    if len(body) > BODY_CAP:
        raise ProbeError("/readyz answered over the body cap")
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
    if gateway == "ready":
        # readiness-shape already proved the status matches the breakdown,
        # so gateway-ready + 503 (e.g. database down) is truthful, not a lie:
        # only the service itself is not ready. Gateway truthfulness and
        # service readiness are separate verdicts.
        note = "gateway ready: READY dispatch committed"
        if status == 200:
            return note
        if expect_ready:
            raise ProbeError(f"{note}, but HTTP {status}: service not ready")
        return f"{note}, but HTTP {status} (truthful, probe stays green)"
    if status == 503 and gateway in ("down", "starting"):
        note = f"gateway {gateway}: parked, not E2E approval"
        if expect_ready:
            raise ProbeError(f"{note}; --expect-ready needs gateway ready")
        return f"{note} (truthful 503, probe stays green)"
    raise ProbeError(f"gateway {gateway!r} disagrees with HTTP {status}")


def _field_age_seconds(entry, field, now_ms):
    """Seconds since the entry's field, or None when absent/non-numeric.

    A timestamp ahead of this clock clamps to 0, never negative.
    """
    last = entry.get(field)
    if isinstance(last, bool) or not isinstance(last, (int, float)):
        return None
    return max(0, int(now_ms - last) // 1000)


def job_age_seconds(entry, now_ms):
    """Seconds since the entry's last_success, or None when unassessable.

    None covers a job with no recorded success, an absent field, or a
    non-numeric value from an older build. Whether that is evidence of stuck
    is decided by check_jobs via consecutive_failures/last_start, not here.
    """
    return _field_age_seconds(entry, "last_success", now_ms)


def consecutive_failures(entry):
    """Non-negative failure count, tolerating absent/malformed values."""
    failures = entry.get("consecutive_failures")
    if isinstance(failures, bool) or not isinstance(failures, (int, float)):
        return 0
    return max(0, int(failures))


def check_jobs(report, now_ms=None, enforce_freshness=True):
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
    if now_ms is None:
        now_ms = int(time.time() * 1000)
    parked_while_degraded = sum(1 for entry in jobs.values()
                                if entry.get("parked"))
    if not enforce_freshness:
        # Degraded without --expect-ready: the probe grades truthfulness, not
        # scheduler progress, so a truthful parked 503 stays green.
        return (f"{len(jobs)} jobs reported while degraded: freshness not "
                f"graded ({parked_while_degraded} parked; informational, "
                f"never gates readiness)")
    stale, warns = [], []
    parked, fresh, not_yet = 0, 0, 0
    ungraded = []
    for name in sorted(jobs):
        entry = jobs[name]
        if entry.get("parked"):
            parked += 1
            continue
        limit = MAX_LAST_SUCCESS_AGE_SECONDS.get(name)
        if limit is None:
            # feeds (env-configured cadence) or a newer build's unknown job:
            # not ours to grade, and never counted as verified fresh.
            ungraded.append(name)
            continue
        age = job_age_seconds(entry, now_ms)
        if age is None:
            # No success recorded yet. A fresh boot (no attempts yet) is not
            # evidence of stuck, but attempts that all failed are: a failed
            # attempt leaves last_success unchanged (jobs.rs), and cutover
            # runs right after a deploy, exactly when last_success is null.
            failures = consecutive_failures(entry)
            if failures > 0:
                detail = f"{name}=no-success {failures} consecutive failures"
                error = entry.get("last_error_class")
                if isinstance(error, str) and error:
                    detail += f" (last error {error})"
                stale.append(detail)
                continue
            started = _field_age_seconds(entry, "last_start", now_ms)
            if started is not None and started > limit:
                stale.append(f"{name}=last_start {started}s ago with no "
                             f"success (>{limit}s)")
                continue
            not_yet += 1
            continue  # never ran yet; not evidence of stuck
        if age > limit:
            stale.append(f"{name}={age}s > {limit}s")
        else:
            fresh += 1
            if age > WARN_LAST_SUCCESS_AGE_SECONDS[name]:
                warns.append(f"{name}={age}s")
    if stale:
        shown = ", ".join(stale)
        raise ProbeError(f"stale jobs past max_last_success_age_seconds or "
                         f"failing with no success: {shown}")
    parts = [f"{fresh} jobs fresh within cadence"]
    if ungraded:
        parts.append(f"{len(ungraded)} ungraded ({', '.join(sorted(ungraded))})")
    if not_yet:
        parts.append(f"{not_yet} not yet run")
    parts.append(f"{parked} parked")
    reason = ", ".join(parts) + " (informational; never gates readiness)"
    if warns:
        reason += f" [warn: {', '.join(warns)}]"
    return reason


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

    if len(readyz_body) > BODY_CAP:
        # Refuse before any JSON parsing: the extra byte never reaches decode.
        oversize = "/readyz answered over the body cap"
        results.append(Result("readiness-shape", False, oversize,
                              f"{SRC_SERVER}, {SRC_HEALTH}"))
        results.append(Result("gateway-state", False,
                              "no truthful breakdown to read gateway from", SRC_GATEWAY))
        results.append(Result("jobs-map", False,
                              "no truthful breakdown to read jobs from", SRC_JOBS))
        results.append(Result("fence-watch", False,
                              "no truthful breakdown to rule out a fence refusal",
                              SRC_FENCE))
        return None, results
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
    enforce = readyz_status == 200 or bool(args.expect_ready)
    record("jobs-map", SRC_JOBS,
           lambda rep: check_jobs(rep, enforce_freshness=enforce), report)
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
