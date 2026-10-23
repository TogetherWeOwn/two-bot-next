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
                   than the fail line) fails and is named. A nonfinite
                   last_success, last_start or consecutive_failures (JSON
                   NaN/Infinity, including an overflowing exponent decoded
                   to Infinity) fails and is named with bounded diagnostics,
                   never a traceback and never a verified-fresh count.
                   Absurd finite magnitudes are display-capped (ages at
                   9,999,999,999 s, failure counts at 999,999,999), far above
                   every fail line, so an in-cap body cannot expand
                   digit-for-digit past the machine output bound; grading is
                   unchanged, since anything past a cap was already stale.
                   While degraded without --expect-ready the jobs map stays
                   informational so a truthful parked 503 stays green. feeds
                   is ungraded (its cadence is env-configured), as is any
                   unknown job name. Parked and ungraded entries skip numeric
                   grading entirely.
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

Duplicate-key refusal contract: every JSON object on both routes is decoded
with duplicate keys refused instead of last-wins, at any depth and after
escape decoding. A repeated key is refused before liveness, readiness-shape,
gateway, jobs or fence grading with the fixed message "/health carries
duplicate JSON object keys" or "/readyz <status> carries duplicate JSON
object keys"; identical repeats and both conflicting orders fail closed, and
unique objects still parse. Refusals leave no receipt and never echo keys,
values, paths or parse exceptions.

Nesting budget: decoded JSON nesting past 32 levels (precedent:
scripts/check_run_record.py MAX_NESTING_DEPTH) is unusable like malformed
JSON, so the route fails an ordinary check. The walk is iterative, so deep
input cannot exhaust the stack, and the verdict never depends on the
interpreter's own recursion limit.

Usage:
  python3 scripts/qa_cutover_probes.py --base-url URL [--expect-ready]
      [--timeout SECONDS] [--evidence FILE] [--json]

Origin admission contract: the base URL must be a usable nonempty bare
http(s) origin before either route is fetched. A refusal names only a
fixed-vocabulary reason (missing, malformed, unusable port, non-http(s)
scheme, credentials, path/query/fragment present, empty host, or unusable
host); it never echoes the rejected URL, not even truncated, and attempts
zero requests.

Build identity contract: /readyz build_revision/build_id must be the
canonical stamped shape (40-char lowercase hex revision, digits-digits
build id; scripts/build_identity.py) or the exact unstamped unknown/unknown
pair. Anything else fails readiness-shape with fixed vocabulary that echoes
neither field.

Remote-field projection contract: only supported values reach diagnostics.
Job names outside the graded cadence map are counted, never listed; a
last_error_class outside the seven JobErrorClass values
(crates/core/src/metrics.rs) is reported as unrecognized, never echoed; a
top-level error payload is reported as present, never quoted. Unknown extra
fields are ignored in both text and evidence output.

Machine verdict contract: --json prints exactly one JSON object on stdout
(verdict, report next_action, and one row per check with stable id, verdict,
code, next_action and detail) with empty stderr. A failed verdict never
claims result=pass and, like text failures, never creates or overwrites
--evidence; the success-only evidence receipt keeps its existing schema.
When the checks pass but the receipt itself cannot be written, the verdict
is FAIL with an additional evidence row (code evidence_not_written): stdout
still holds exactly one JSON object, stderr stays empty, and the exit code
is 1, so a verdict consumer never proceeds as if attested.
"""

import argparse
import http.client
import json
import math
import os
import sys
import time
from dataclasses import dataclass
from urllib.parse import urlsplit
import urllib.error
import urllib.request

from build_identity import (
    UNSTAMPED,
    is_build_id_shape,
    is_build_revision_shape,
)

BODY_CAP = 64 << 10
DEFAULT_TIMEOUT_SECONDS = 10
# Per-request timeout cap: --timeout must be finite and within (0, 60].
# 60 s is 6x the 10 s default and covers the slowest healthy surface;
# anything larger is a misconfiguration, not a slower check.
TIMEOUT_MAX_SECONDS = 60
# Cloudflare rejects the default "Python-urllib/x.y" agent at the edge with 403
# (error 1010), so an explicit agent is required for the probe to reach the Worker.
USER_AGENT = "two-bot-next-staging-rollout/1.0"
KNOWN_STATUSES = ("ready", "starting", "down")
REQUIRED_COMPONENTS = ("process", "gateway")
# Per-surface allowlist for this probe: the container's full breakdown is
# process/gateway (required here) plus database/token_invalid (legitimate
# optional; older fixtures may omit them). Anything else is refused with a
# fixed-vocabulary error that names no component data.
KNOWN_COMPONENTS = frozenset({"process", "gateway", "database", "token_invalid"})
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
#   self_role_recovery: 30s cadence + 25s timeout (recovery_job,
#     self_role_handlers.rs:303-310) -> warn 55, max 85
#   automod_expiry: 60s cadence (EXPIRY_CADENCE, automod_gateway.rs:44) + 5s
#     timeout (expiry_job, automod_gateway.rs:339-344) -> warn 65, max 125
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
    "self_role_recovery": 30,
    "automod_expiry": 60,
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
    "self_role_recovery": 25,
    "automod_expiry": 5,
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
# Display caps for remote-derived magnitudes. The largest graded fail line is
# 7320 s (inactivity/presence_probe) and failure counts are small ints, so
# capping here never changes a grading decision: anything past a cap was
# already stale or failing. Without the caps a single in-cap body carrying
# e.g. last_success=-(10**4200) would expand digit-for-digit into diagnostics
# past the 16 KiB machine output bound.
MAX_DISPLAY_AGE_SECONDS = 9_999_999_999
MAX_DISPLAY_FAILURES = 999_999_999
# Only these last_error_class values may reach diagnostics; anything else is
# reported as unrecognized, never echoed. Source: JobErrorClass,
# crates/core/src/metrics.rs (serde snake_case); null means no failure yet.
JOB_ERROR_CLASSES = frozenset({
    "database", "rest", "configuration", "timeout", "panic", "feed",
    "recovery_required",
})
# Fallback machine code/action for a ProbeError raised without either. Every
# raise site in this module passes explicit values; the defaults only keep
# direct constructions (and older callers) on the verdict path.
DEFAULT_FAIL_CODE = "check_failed"
DEFAULT_FAIL_ACTION = "inspect the failing check and re-run the probe"
CHECK_PASSED_ACTION = "no action needed"


class ProbeError(Exception):
    """A check's one-line failure reason, with a stable machine code.

    The message is fixed vocabulary: it never carries remote input. The code
    is a bounded lowercase token and next_action a short remediation; both
    are stable across equivalent failures and feed the --json verdict rows.
    """

    def __init__(self, message, code=DEFAULT_FAIL_CODE,
                 next_action=DEFAULT_FAIL_ACTION):
        super().__init__(message)
        self.code = code
        self.next_action = next_action


class DuplicateKeyError(ValueError):
    """A JSON object repeats a key; the message never carries key/value data."""


def _reject_duplicate_keys(pairs):
    """object_pairs_hook refusing repeats instead of last-wins.

    The json decoder calls this for every object at any depth, after
    escape decoding, so {"a": 1, "\\u0061": 2} counts as a repeat. The
    raised message is fixed vocabulary: it names no key, value or path.
    """
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise DuplicateKeyError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


def decode_strict(body):
    """Decode JSON while refusing duplicate object keys at any depth."""
    return json.loads(body, object_pairs_hook=_reject_duplicate_keys)


@dataclass(frozen=True)
class Result:
    name: str
    ok: bool
    reason: str
    source: str
    code: str = DEFAULT_FAIL_CODE
    next_action: str = DEFAULT_FAIL_ACTION

    def line(self):
        return f"{'PASS' if self.ok else 'FAIL'} {self.name}: {self.reason} [{self.source}]"

    def verdict_row(self):
        return {"id": self.name,
                "verdict": "PASS" if self.ok else "FAIL",
                "code": self.code,
                "next_action": self.next_action,
                "detail": self.reason}


SRC_SERVER = "crates/bot/src/server.rs"
SRC_HEALTH = "crates/core/src/health.rs"
SRC_GATEWAY = "crates/bot/src/gateway.rs"
SRC_JOBS = "crates/bot/src/jobs.rs"
SRC_FENCE = "wrangler/src/ownership.ts"


def valid_timeout(value):
    """A --timeout is usable only when finite and within (0, 60]."""
    return math.isfinite(value) and 0 < value <= TIMEOUT_MAX_SECONDS


def normalize_base(url):
    """Accept a usable nonempty bare http(s) origin; probes append paths.

    Every refusal is fixed vocabulary with its own code: the rejected URL is
    never echoed, not even truncated, and no request is attempted.
    """
    if not url:
        raise ProbeError("no base URL given (--base-url or QA_PROBE_BASE_URL)",
                         code="origin_missing",
                         next_action="pass a bare http(s) origin as "
                                     "--base-url or QA_PROBE_BASE_URL")
    try:
        parts = urlsplit(url)
    except ValueError:
        raise ProbeError("refusing: base URL is malformed",
                         code="origin_malformed",
                         next_action="pass a bare http(s) origin such as "
                                     "https://staging.example")
    try:
        port = parts.port
    except ValueError:
        raise ProbeError("refusing: base URL has an unusable port",
                         code="origin_bad_port",
                         next_action="pass a bare http(s) origin with a usable "
                                     "port 1-65535 or no port")
    if port is not None and not 1 <= port <= 65535:
        raise ProbeError("refusing: base URL has an unusable port",
                         code="origin_bad_port",
                         next_action="pass a bare http(s) origin with a usable "
                                     "port 1-65535 or no port")
    if parts.scheme not in ("http", "https"):
        raise ProbeError("refusing: base URL is not http(s)",
                         code="origin_bad_scheme",
                         next_action="pass a bare http or https origin as --base-url")
    if parts.username or parts.password:
        raise ProbeError("refusing: base URL must not carry credentials",
                         code="origin_has_credentials",
                         next_action="remove credentials from --base-url; "
                                     "probes send no credentials")
    if parts.path not in ("", "/") or parts.query or parts.fragment:
        raise ProbeError("refusing: base URL must be a bare origin, no path",
                         code="origin_not_bare",
                         next_action="pass a bare origin without path, query "
                                     "or fragment")
    host = parts.hostname or ""
    if not host:
        raise ProbeError("refusing: base URL has no host",
                         code="origin_missing_host",
                         next_action="pass a bare http(s) origin with a nonempty host")
    try:
        host.encode("idna")
    except UnicodeError:
        # Mirrors the IDNA encoding socket.getaddrinfo applies before any
        # packet: an overlong label would otherwise escape the fetch guards
        # (UnicodeError is a ValueError, not an OSError) with a traceback.
        raise ProbeError("refusing: base URL host is not usable",
                         code="origin_bad_host",
                         next_action="pass a bare http(s) origin with a usable "
                                     "DNS host") from None
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    port_suffix = f":{port}" if port else ""
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


# Keeps every decoded walk far below the interpreter's recursion limit.
# Legitimate route shapes nest about 4 levels; anything past 32 is refused
# as unusable, like malformed JSON.
MAX_NESTING_DEPTH = 32


def exceeds_nesting_budget(value):
    """True when any container nests deeper than MAX_NESTING_DEPTH levels.

    Iterative, so deep input cannot exhaust the stack.
    """
    pending = [(value, 1)]
    while pending:
        node, level = pending.pop()
        if isinstance(node, dict):
            children = list(node.values())
        elif isinstance(node, list):
            children = node
        else:
            continue
        if level > MAX_NESTING_DEPTH:
            return True
        pending.extend((child, level + 1) for child in children)
    return False


def decode(body):
    """Decode a route body, or None when it is not usable JSON.

    Duplicate keys stay a loud refusal; malformed JSON, a decoder
    RecursionError (not a ValueError), and input nesting past the budget
    become None so the caller fails an ordinary check instead of escaping
    the verdict path.
    """
    try:
        parsed = decode_strict(body)
    except DuplicateKeyError:
        raise
    except (ValueError, UnicodeDecodeError, RecursionError):
        return None
    if exceeds_nesting_budget(parsed):
        return None
    return parsed


def canonical_build_identity(report):
    """Return a canonical stamped identity or the exact unstamped pair."""
    if not isinstance(report, dict):
        return None
    revision, build_id = report.get("build_revision"), report.get("build_id")
    if revision == UNSTAMPED and build_id == UNSTAMPED:
        return revision, build_id
    if is_build_revision_shape(revision) and is_build_id_shape(build_id):
        return revision, build_id
    return None


def check_liveness(status, body):
    if status != 200:
        raise ProbeError(f"/health answered {status}, expected 200",
                         code="liveness_bad_status",
                         next_action="inspect the target /health route and "
                                     "re-run the probe")
    if len(body) > BODY_CAP:
        raise ProbeError("/health answered over the body cap",
                         code="liveness_over_body_cap",
                         next_action="inspect why /health answered over the "
                                     "64 KiB body cap")
    try:
        parsed = decode(body)
    except DuplicateKeyError:
        raise ProbeError("/health carries duplicate JSON object keys",
                         code="liveness_duplicate_keys",
                         next_action="inspect why /health carries duplicate "
                                     "JSON object keys")
    if parsed != {"status": "ok"}:
        raise ProbeError("/health 200 without {\"status\": \"ok\"}",
                         code="liveness_bad_shape",
                         next_action="inspect the target /health route and "
                                     "re-run the probe")
    return "/health 200 {\"status\": \"ok\"}: process answers HTTP"


def check_readyz_shape(status, body):
    inspect_breakdown = ("inspect the target /readyz breakdown and re-run "
                         "the probe")
    if len(body) > BODY_CAP:
        raise ProbeError("/readyz answered over the body cap",
                         code="readyz_over_body_cap",
                         next_action="inspect why /readyz answered over the "
                                     "64 KiB body cap")
    try:
        report = decode(body)
    except DuplicateKeyError:
        raise ProbeError(f"/readyz {status} carries duplicate JSON object keys",
                         code="readyz_duplicate_keys",
                         next_action="inspect why /readyz carries duplicate "
                                     "JSON object keys")
    if isinstance(report, dict) and report.get("error"):
        raise ProbeError("/readyz refused with an error field (ownership "
                         "fence or proxy refusal, not the container breakdown)",
                         code="readyz_refused_error",
                         next_action="read Worker ownership state; this is not "
                                     "the container breakdown")
    if status in (301, 302, 303, 307, 308):
        raise ProbeError(f"/readyz answered {status}: refusing to follow a redirect",
                         code="readyz_redirect",
                         next_action="probe the container origin directly; "
                                     "redirects are never followed")
    components = report.get("components") if isinstance(report, dict) else None
    if (not isinstance(components, list) or not components
            or not all(isinstance(c, list) and len(c) == 2
                       and isinstance(c[0], str) and c[0]
                       and c[1] in KNOWN_STATUSES for c in components)):
        raise ProbeError(f"/readyz {status} without the components breakdown "
                         "(expected [[name, ready|starting|down], ...])",
                         code="readyz_bad_shape",
                         next_action=inspect_breakdown)
    # Reject duplicates before any readiness math: last-wins would let a
    # later ready mask an earlier down.
    names = [c[0] for c in components]
    if len(set(names)) != len(names):
        raise ProbeError(f"/readyz {status} repeats a component name",
                         code="readyz_duplicate_component",
                         next_action=inspect_breakdown)
    if any(name not in KNOWN_COMPONENTS for name in names):
        raise ProbeError(f"/readyz {status} carries an unknown component name",
                         code="readyz_unknown_component",
                         next_action=inspect_breakdown)
    state = dict(components)
    missing = [name for name in REQUIRED_COMPONENTS if name not in state]
    if missing:
        raise ProbeError(f"/readyz breakdown lacks {', '.join(missing)}",
                         code="readyz_missing_component",
                         next_action=inspect_breakdown)
    for key in ("build_revision", "build_id"):
        if not isinstance(report.get(key), str):
            raise ProbeError(f"/readyz breakdown lacks {key!r} build identity",
                             code="readyz_missing_build_identity",
                             next_action="inspect why /readyz omits "
                                         "build_revision/build_id")
    if canonical_build_identity(report) is None:
        raise ProbeError("/readyz build identity is not the canonical stamped "
                         "shape or the exact unknown pair (expected 40-hex "
                         "build_revision and digits-digits build_id)",
                         code="readyz_bad_build_identity",
                         next_action="inspect why /readyz build identity is "
                                     "not canonical")
    ready = all(value == "ready" for value in state.values())
    expected = 200 if ready else 503
    if status != expected:
        raise ProbeError(f"/readyz {status} disagrees with its breakdown "
                         f"({'all ready' if ready else 'not all ready'}, expected {expected})",
                         code="readyz_status_disagrees",
                         next_action="inspect why the /readyz status disagrees "
                                     "with its breakdown")
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
            raise ProbeError(f"{note}, but HTTP {status}: service not ready",
                             code="gateway_service_not_ready",
                             next_action="inspect the down components, then "
                                         "re-run with --expect-ready once 200")
        return f"{note}, but HTTP {status} (truthful, probe stays green)"
    if status == 503 and gateway in ("down", "starting"):
        note = f"gateway {gateway}: parked, not E2E approval"
        if expect_ready:
            raise ProbeError(f"{note}; --expect-ready needs gateway ready",
                             code="gateway_parked",
                             next_action="start the gateway prerequisites, or "
                                         "drop --expect-ready to grade "
                                         "truthfulness only")
        return f"{note} (truthful 503, probe stays green)"
    raise ProbeError(f"gateway {gateway!r} disagrees with HTTP {status}",
                     code="gateway_disagrees",
                     next_action="inspect why the gateway state disagrees "
                                 "with the HTTP status")


def _field_age_seconds(entry, field, now_ms):
    """Seconds since the entry's field, or None when absent/non-numeric.

    A timestamp ahead of this clock clamps to 0, never negative. A
    nonfinite float (JSON NaN/Infinity, including an overflowing exponent
    decoded to Infinity) raises ProbeError: freshness is unassessable,
    never fresh and never a traceback.
    """
    last = entry.get(field)
    if isinstance(last, bool) or not isinstance(last, (int, float)):
        return None
    if isinstance(last, float) and not math.isfinite(last):
        raise ProbeError(f"nonfinite {field} (refusing to grade freshness "
                         f"from NaN/Infinity)",
                         code="jobs_nonfinite_field",
                         next_action="inspect why the job timestamp is not "
                                     "finite and re-run the probe")
    return min(MAX_DISPLAY_AGE_SECONDS, max(0, int(now_ms - last) // 1000))


def job_age_seconds(entry, now_ms):
    """Seconds since the entry's last_success, or None when unassessable.

    None covers a job with no recorded success, an absent field, or a
    non-numeric value from an older build. Whether that is evidence of stuck
    is decided by check_jobs via consecutive_failures/last_start, not here.
    """
    return _field_age_seconds(entry, "last_success", now_ms)


def consecutive_failures(entry):
    """Non-negative failure count, tolerating absent/malformed values.

    A nonfinite float (JSON NaN/Infinity, including an overflowing
    exponent decoded to Infinity) raises ProbeError: the count is
    unassessable, never zero and never a traceback.
    """
    failures = entry.get("consecutive_failures")
    if isinstance(failures, bool) or not isinstance(failures, (int, float)):
        return 0
    if isinstance(failures, float) and not math.isfinite(failures):
        raise ProbeError("nonfinite consecutive_failures (refusing to grade "
                         "freshness from NaN/Infinity)",
                         code="jobs_nonfinite_field",
                         next_action="inspect why the failure count is not "
                                     "finite and re-run the probe")
    return min(MAX_DISPLAY_FAILURES, max(0, int(failures)))


def check_jobs(report, now_ms=None, enforce_freshness=True):
    """Grade the informational jobs map, naming only graded job names.

    Unknown job names are remote input: they stay ungraded and are counted,
    never listed. Only names with a known cadence constant reach diagnostics.
    """
    jobs = report.get("jobs")
    if not isinstance(jobs, dict) or not jobs:
        raise ProbeError("/readyz carries no informational jobs map",
                         code="jobs_missing_map",
                         next_action="inspect why /readyz omits the "
                                     "informational jobs map")
    bad = [name for name, entry in jobs.items()
           if not isinstance(entry, dict)
           or not isinstance(entry.get("parked"), bool)
           or not isinstance(entry.get("running"), bool)]
    if bad:
        raise ProbeError(f"/readyz jobs map has {len(bad)} malformed entries",
                         code="jobs_malformed_entries",
                         next_action="inspect why /readyz job entries lack "
                                     "parked/running booleans")
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
        # Nonfinite floats (JSON NaN/Infinity, including overflowing
        # exponents decoded to Infinity) are unassessable freshness: fail
        # the job with bounded diagnostics naming the job and field, never
        # a traceback and never a verified-fresh count. Parked and ungraded
        # jobs above stay exempt; absent and finite values flow below.
        nonfinite_field = None
        for field in ("last_success", "last_start", "consecutive_failures"):
            value = entry.get(field)
            if isinstance(value, float) and not math.isfinite(value):
                nonfinite_field = field
                break
        if nonfinite_field is not None:
            stale.append(f"{name} has nonfinite {nonfinite_field} "
                         f"(unassessable freshness)")
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
                if isinstance(error, str) and error in JOB_ERROR_CLASSES:
                    detail += f" (last error {error})"
                elif isinstance(error, str) and error:
                    detail += " (last error class unrecognized)"
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
                         f"failing with no success: {shown}",
                         code="jobs_stale_or_failing",
                         next_action="inspect the failing jobs on the target "
                                     "and re-run the probe")
    parts = [f"{fresh} jobs fresh within cadence"]
    if ungraded:
        parts.append(f"{len(ungraded)} ungraded")
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
    shape_source = f"{SRC_SERVER}, {SRC_HEALTH}"
    no_breakdown = "fix the readiness-shape failure first"

    def record(name, source, check, *check_args, ok_code):
        try:
            results.append(Result(name, True, check(*check_args), source,
                                  ok_code, CHECK_PASSED_ACTION))
        except ProbeError as e:
            results.append(Result(name, False, str(e), source,
                                  e.code, e.next_action))

    try:
        base = normalize_base(args.base_url)
    except ProbeError as e:
        return None, [Result("base-url", False, str(e),
                             "scripts/qa_cutover_probes.py", e.code, e.next_action)]

    fetch_one = fetch_fn if fetch_fn is not None else (lambda url: fetch(url, args.timeout))
    unreachable = "check the probe target is reachable and re-run the probe"
    try:
        health_status, health_body = fetch_one(base + "/health")
    except (OSError, http.client.HTTPException, UnicodeError) as e:
        return None, [Result("liveness", False,
                             f"/health did not respond ({e.__class__.__name__})",
                             SRC_SERVER, "liveness_no_response", unreachable)]
    try:
        readyz_status, readyz_body = fetch_one(base + "/readyz")
    except (OSError, http.client.HTTPException, UnicodeError) as e:
        record("liveness", SRC_SERVER, check_liveness, health_status,
               health_body, ok_code="liveness_ok")
        results.append(Result("readiness-shape", False,
                              f"/readyz did not respond ({e.__class__.__name__})",
                              shape_source, "readyz_no_response", unreachable))
        return None, results
    # Liveness first: it is independent of the readyz body.
    record("liveness", SRC_SERVER, check_liveness, health_status, health_body,
           ok_code="liveness_ok")

    if len(readyz_body) > BODY_CAP:
        # Refuse before any JSON parsing: the extra byte never reaches decode.
        oversize = "/readyz answered over the body cap"
        oversize_action = ("inspect why /readyz answered over the 64 KiB "
                           "body cap")
        results.append(Result("readiness-shape", False, oversize, shape_source,
                              "readyz_over_body_cap", oversize_action))
        results.append(Result("gateway-state", False,
                              "no truthful breakdown to read gateway from", SRC_GATEWAY,
                              "gateway_no_breakdown", no_breakdown))
        results.append(Result("jobs-map", False,
                              "no truthful breakdown to read jobs from", SRC_JOBS,
                              "jobs_no_breakdown", no_breakdown))
        results.append(Result("fence-watch", False,
                              "no truthful breakdown to rule out a fence refusal",
                              SRC_FENCE, "fence_no_breakdown", no_breakdown))
        return None, results
    try:
        report = decode(readyz_body)
    except DuplicateKeyError:
        # No last-wins breakdown survives: grading sees no report, the
        # refusal below fails closed, and no receipt is written.
        report = None
    try:
        state, shape_reason = check_readyz_shape(readyz_status, readyz_body)
    except ProbeError as e:
        results.append(Result("readiness-shape", False, str(e), shape_source,
                              e.code, e.next_action))
        results.append(Result("gateway-state", False,
                              "no truthful breakdown to read gateway from", SRC_GATEWAY,
                              "gateway_no_breakdown", no_breakdown))
        results.append(Result("jobs-map", False,
                              "no truthful breakdown to read jobs from", SRC_JOBS,
                              "jobs_no_breakdown", no_breakdown))
        if isinstance(report, dict) and report.get("error"):
            results.append(Result("fence-watch", False, str(e), SRC_FENCE,
                                  "fence_refusal_present",
                                  "read Worker ownership state; this is not "
                                  "the container breakdown"))
        else:
            results.append(Result("fence-watch", False,
                                  "no truthful breakdown to rule out a fence refusal",
                                  SRC_FENCE, "fence_no_breakdown", no_breakdown))
        return report, results
    results.append(Result("readiness-shape", True, shape_reason, shape_source,
                          "readiness_shape_ok", CHECK_PASSED_ACTION))
    record("gateway-state", SRC_GATEWAY, check_gateway, state,
           readyz_status, args.expect_ready, ok_code="gateway_state_ok")
    enforce = readyz_status == 200 or bool(args.expect_ready)
    record("jobs-map", SRC_JOBS,
           lambda rep: check_jobs(rep, enforce_freshness=enforce), report,
           ok_code="jobs_map_ok")
    if isinstance(report, dict) and report.get("error"):
        results.append(Result("fence-watch", False,
                              "container breakdown carries an error field",
                              SRC_FENCE, "fence_error_present",
                              "read Worker ownership state; this is not "
                              "the container breakdown"))
    else:
        results.append(Result("fence-watch", True,
                              "container breakdown, no ownership refusal", SRC_FENCE,
                              "fence_watch_ok", CHECK_PASSED_ACTION))
    return report, results


JSON_PASS_ACTION = "all cutover probes passed: proceed with the cutover gate"
JSON_FAIL_ACTION = ("one or more cutover probes failed: fix the target and "
                    "re-run the probe")


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--base-url", default=os.environ.get("QA_PROBE_BASE_URL"),
                        help="bare http(s) origin to probe (default: $QA_PROBE_BASE_URL)")
    parser.add_argument("--expect-ready", action="store_true",
                        help="fail gateway-state unless the service is ready "
                             "(default: a truthful parked 503 stays green)")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS,
                        help=f"seconds per GET, finite and within (0, 60] "
                             f"(default: {DEFAULT_TIMEOUT_SECONDS})")
    parser.add_argument("--evidence", default=None,
                        help="receipt path written when both routes answer")
    parser.add_argument("--json", action="store_true",
                        help="print one machine-readable JSON verdict object "
                             "instead of text lines")
    return parser.parse_args(argv)


def main(argv=None, fetch_fn=None):
    args = parse_args(argv)
    if not valid_timeout(args.timeout):
        print("qa_cutover_probes: --timeout must be a finite number of seconds "
              "greater than 0 and at most 60", file=sys.stderr)
        return 2
    report, results = run(args, fetch_fn)
    passed = report is not None and all(r.ok for r in results)
    evidence_error = None
    if passed and args.evidence:
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
            evidence_error = error.__class__.__name__
    if args.json:
        rows = [r.verdict_row() for r in results]
        if evidence_error is not None:
            rows.append({
                "id": "evidence",
                "verdict": "FAIL",
                "code": "evidence_not_written",
                "next_action": ("check the --evidence path is writable "
                                "and re-run the probe"),
                "detail": (f"evidence receipt not written "
                           f"({evidence_error})"),
            })
        verdict = "PASS" if passed and evidence_error is None else "FAIL"
        print(json.dumps({
            "verdict": verdict,
            "next_action": JSON_PASS_ACTION if verdict == "PASS" else JSON_FAIL_ACTION,
            "checks": rows,
        }, indent=2))
    else:
        for result in results:
            print(result.line())
        failed = sum(not r.ok for r in results)
        print(f"cutover acceptance probes: {len(results) - failed}/{len(results)} checks passed")
    if evidence_error is not None:
        # The verdict above already carries the failure in --json mode, so
        # stderr stays empty there; text mode keeps the one-line notice.
        if not args.json:
            print(f"cutover acceptance probes failed: evidence not written "
                  f"({evidence_error})")
        return 1
    if not passed:
        # Refusals and failures leave no receipt to attest: --evidence is
        # neither created nor overwritten here.
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
