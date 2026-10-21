#!/usr/bin/env python3
"""Read-only staging health-contract probe for the bot container (stdlib only).

Two GETs against the staging Worker origin, each traced to the Rust contract
(`crates/bot/src/server.rs`, `crates/core/src/health.rs`):

  health     GET /health answers 200 {"status":"ok"}. Process liveness only:
             always 200 while the process answers, never a readiness claim.
  readyz     GET /readyz answers 200 or 503 with the readiness breakdown:
             components (process/gateway/database/token_invalid, each
             ready/starting/down), informational jobs, an optional
             gateway_failure {phase, class} in the fixed vocabulary, and the
             compiled build_revision/build_id.

The probe asserts the shape, the status-code mapping (200 if and only if
every component is ready), and the build-identity fields. A gateway_failure
class of checkpoint_load_failed reports only the failed checkpoint read;
its root cause needs separate reviewed schema/ACL/connectivity evidence
(docs/voice-cutover-rollback-triggers.md T1). A shape-correct 503 is still a
FAIL: a truthful parked process is never E2E approval.

No probe may apply a migration or test production. This probe never writes,
follows no redirects, and sends no credentials. The only network calls
are the two staging GETs above.

Body-cap refusal contract: the transport reads at most BODY_CAP+1 bytes on
both the normal and HTTPError paths so a hidden suffix is detected. Any body
longer than BODY_CAP (64 KiB) is refused before JSON parsing with the fixed
message "<name> probe answered over the body cap"; exact-cap valid input
passes, and the extra byte never reaches a successful JSON path. Refusals
never echo the body, headers, URL or parse exceptions.

Origin fence (fail-closed, before any request): only an
https://two-bot-next-staging.<sub>.workers.dev origin (no port, userinfo,
path, query or fragment) is fetched; anything else -- including the
production Worker -- refuses with exit 2 and no request is sent.

Usage:
  STAGING_WORKER_URL=https://two-bot-next-staging.<sub>.workers.dev \\
      python3 scripts/staging_health_contract_probe.py [--evidence FILE] [--expected-sha SHA]
"""

import argparse
from dataclasses import dataclass
import http.client
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit

from build_identity import UNSTAMPED, is_build_id_shape, is_build_revision_shape

STAGING_HOST = re.compile(r"two-bot-next-staging\.[a-z0-9-]+\.workers\.dev")
BODY_CAP = 64 << 10
TIMEOUT_SECONDS = 10
USER_AGENT = "two-bot-next-staging-health-contract-probe/1.0 (read-only E2E)"

REQUIRED_COMPONENTS = ("process", "gateway", "database", "token_invalid")
# Per-surface allowlist: exactly the four components the container serves
# (crates/bot/src/server.rs readiness_report + with_token_state). No
# optionals on this surface; anything else is refused with fixed vocabulary
# naming no component data.
ALLOWED_COMPONENTS = frozenset({"process", "gateway", "database", "token_invalid"})
STATUSES = ("ready", "starting", "down")
FAILURE_PHASE = "durable_gateway"
FAILURE_CLASSES = frozenset({
    "store_unavailable",
    "gateway_pool_connect_failed",
    "checkpoint_load_failed",
    "onboarding_gates_invalid",
    "onboarding_init_failed",
    "custom_commands_init_failed",
    "milestones_load_failed",
    "automod_config_invalid",
    "automod_executor_failed",
    "raid_executor_failed",
    "gateway_runtime_failed",
    "gateway_task_panicked",
})
# This class records the failed read step, not its schema/ACL/connectivity cause.
CHECKPOINT_READ_CLASS = "checkpoint_load_failed"


class ProbeError(Exception):
    """A check's one-line failure reason (fixed vocabulary plus observed values)."""


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


@dataclass(frozen=True)
class Result:
    name: str
    ok: bool
    reason: str
    checkpoint_read_failed: bool = False

    def line(self):
        return f"{'PASS' if self.ok else 'FAIL'} {self.name}: {self.reason}"


def staging_origin(url):
    """Allowlist the staging Worker origin; refuse everything else unread."""
    if not url:
        raise ProbeError("no staging Worker URL given (--staging-url or STAGING_WORKER_URL)")
    try:
        parts = urlsplit(url)
    except ValueError:
        raise ProbeError("refusing: not the two-bot-next-staging workers.dev origin")
    try:
        port = parts.port
    except ValueError:
        port = "invalid"
    if (parts.scheme != "https" or port is not None or parts.username or parts.password
            or not STAGING_HOST.fullmatch(parts.hostname or "")
            or parts.path not in ("", "/") or parts.query or parts.fragment):
        raise ProbeError("refusing: not the two-bot-next-staging workers.dev origin")
    return f"https://{parts.hostname}"


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None  # urllib then raises the 3xx as an HTTPError


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


def get_json(fetch_fn, url, name):
    """One GET returning (status, headers, parsed JSON or None)."""
    try:
        status, headers, body = fetch_fn(url)
    except (OSError, http.client.HTTPException) as error:
        raise ProbeError(f"{name} probe did not respond ({error.__class__.__name__})")
    if len(body) > BODY_CAP:
        raise ProbeError(f"{name} probe answered over the body cap")
    try:
        value = json.loads(body, object_pairs_hook=_reject_duplicate_keys)
    except DuplicateKeyError:
        raise ProbeError(f"{name} probe carries duplicate JSON object keys") from None
    except (ValueError, UnicodeDecodeError, RecursionError):
        # Deeply nested in-cap JSON exhausts the decoder recursion limit;
        # refuse it as malformed, same as any other unparseable body.
        value = None
    return status, headers, value


def check_health(origin, fetch_fn):
    status, headers, body = get_json(fetch_fn, origin + "/health", "health")
    if status != 200:
        raise ProbeError(f"health answered {status}, expected the exact 200 liveness")
    if body != {"status": "ok"}:
        raise ProbeError("health 200 without the exact status shape")
    if header(headers, "location") is not None:
        raise ProbeError("health 200 carries a redirect target")
    return "health 200 with the exact liveness shape"


def parse_readyz(status, value):
    """Validate the readiness breakdown; return (state dict, report)."""
    if not isinstance(value, dict) or value.get("error") is not None:
        raise ProbeError(f"readyz {status} without the bot's component breakdown "
                         "(ownership refusal or broken deploy)")
    components = value.get("components")
    if (not isinstance(components, list) or not components
            or not all(isinstance(row, list) and len(row) == 2
                       and isinstance(row[0], str) and row[0]
                       and row[1] in STATUSES for row in components)):
        raise ProbeError(f"readyz {status} without the bot's component breakdown "
                         "(ownership refusal or broken deploy)")
    # Reject duplicates before any readiness math: last-wins would let a
    # later ready mask an earlier down.
    names = [row[0] for row in components]
    if len(set(names)) != len(names):
        raise ProbeError(f"readyz {status} repeats a component name")
    if any(name not in ALLOWED_COMPONENTS for name in names):
        raise ProbeError(f"readyz {status} carries an unknown component name")
    state = dict(components)
    missing = [name for name in REQUIRED_COMPONENTS if name not in state]
    if missing:
        raise ProbeError(f"readyz {status} without the bot's component breakdown "
                         f"(missing {', '.join(missing)})")
    jobs = value.get("jobs")
    if not isinstance(jobs, dict):
        raise ProbeError(f"readyz {status} without the informational jobs object")
    failure = value.get("gateway_failure")
    if failure is not None:
        if (not isinstance(failure, dict) or failure.get("phase") != FAILURE_PHASE
                or not isinstance(failure.get("class"), str)
                or failure.get("class") not in FAILURE_CLASSES):
            raise ProbeError(f"readyz {status} with a gateway failure outside the fixed vocabulary")
    return state, value


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


def check_build(report, expected_sha):
    if expected_sha is not None and not is_build_revision_shape(expected_sha):
        return Result("build", False, "expected SHA must be a canonical 40-character lowercase hex commit")
    if not isinstance(report, dict):
        return Result("build", False, "no readyz body to read build identity from")
    identity = canonical_build_identity(report)
    if identity is None:
        return Result("build", False, "readyz without the compiled build_revision/build_id "
                      "in the canonical shape")
    revision, build_id = identity
    if revision == UNSTAMPED:
        return Result("build", expected_sha is None,
                      "build identity is unstamped (unknown)" +
                      ("; cannot match the expected SHA" if expected_sha is not None else ""))
    if expected_sha is not None and revision != expected_sha:
        return Result("build", False,
                      f"build_revision {revision} does not match the expected SHA")
    detail = f"build_revision {revision} build_id {build_id}"
    if expected_sha is not None:
        detail += " matches the expected SHA"
    return Result("build", True, detail)


def run(args, fetch_fn=fetch):
    try:
        origin = staging_origin(args.staging_url)
        if args.expected_sha is not None and not is_build_revision_shape(args.expected_sha):
            raise ProbeError("expected SHA must be a canonical 40-character lowercase hex commit")
    except ProbeError as error:
        return None, [Result("origin", False, str(error))], None
    results = []
    try:
        results.append(Result("health", True, check_health(origin, fetch_fn)))
    except ProbeError as error:
        results.append(Result("health", False, str(error)))
    report = None
    try:
        status, _, value = get_json(fetch_fn, origin + "/readyz", "readyz")
        state, report = parse_readyz(status, value)
        expected = 200 if all(text == "ready" for text in state.values()) else 503
        if status != expected:
            raise ProbeError(f"readyz {status} contradicts the component breakdown "
                             f"(expected {expected})")
        if status == 200:
            results.append(Result("readyz", True, "readyz 200: every component ready"))
        else:
            down = sorted(name for name, text in state.items() if text != "ready")
            failure = value.get("gateway_failure") or {}
            if failure.get("class") == CHECKPOINT_READ_CLASS:
                results.append(Result("readyz", False,
                                      "readyz 503: checkpoint read failed "
                                      f"(gateway_failure durable_gateway:{CHECKPOINT_READ_CLASS}); "
                                      "root cause unverified; require separate reviewed "
                                      "schema/ACL/connectivity evidence",
                                      checkpoint_read_failed=True))
            else:
                results.append(Result("readyz", False,
                                      f"readyz 503: parked ({', '.join(down)} not ready); "
                                      "truthful, not E2E approval"))
    except ProbeError as error:
        results.append(Result("readyz", False, str(error)))
    results.append(check_build(report, args.expected_sha))
    return origin, results, report


def evidence_shape(results, report, expected_sha):
    """Allowlisted receipt: check names, verdicts and shape tokens only."""
    failure = (report or {}).get("gateway_failure") or {}
    identity = canonical_build_identity(report)
    return {"checks": [{"check": result.name, "verdict": "pass" if result.ok else "fail",
                        "shape": result.reason} for result in results],
            "build_revision": identity[0] if identity else None,
            "build_id": identity[1] if identity else None,
            "gateway_failure_class": failure.get("class"),
            "gateway_failure_root_cause": ("unverified"
                                           if any(result.checkpoint_read_failed
                                                  for result in results)
                                           else None),
            "expected_sha": expected_sha if is_build_revision_shape(expected_sha) else None}


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--staging-url",
                        default=os.environ.get("STAGING_WORKER_URL"),
                        help="staging Worker origin (default: $STAGING_WORKER_URL)")
    parser.add_argument("--expected-sha", default=os.environ.get("EXPECTED_SHA"),
                        help="deployed commit SHA the binary must report (default: $EXPECTED_SHA)")
    parser.add_argument("--evidence", default="staging-health-contract-evidence.json",
                        help="receipt path written when the fetch phase completes")
    return parser.parse_args(argv)


def main(argv=None, fetch_fn=fetch):
    args = parse_args(argv)
    origin, results, report = run(args, fetch_fn)
    for result in results:
        print(result.line())
    failed = sum(not result.ok for result in results)
    print(f"staging health-contract probe: {len(results) - failed}/{len(results)} checks passed")
    if origin is None:
        # Origin/input fence refusal: no request was sent, nothing to attest.
        return 2
    receipt = {"origin": origin,
               "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               **evidence_shape(results, report, args.expected_sha),
               "result": "pass" if not failed else "fail"}
    try:
        with open(args.evidence, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=2)
            handle.write("\n")
    except OSError as error:
        print(f"staging health-contract probe failed: evidence not written ({error.__class__.__name__})")
        return 1
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
