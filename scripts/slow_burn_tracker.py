#!/usr/bin/env python3
"""File, refresh or resolve the creator-join slow-burn tracking issue.

The `voice_join_slow_burn` Worker rule only alerts: this scheduled tracker
turns the first firing into one GitHub tracking issue and closes it when the
burn clears. While the burn keeps firing the open issue is left alone (a
refresh comment lands at most every six hours), so a refire never duplicates
the card. An unknown verdict (failed scrape, failed evaluation, unusable
join series, or slow windows without full history) never closes an open
card: silence is not recovery.

The firing verdict comes from the checked-in evaluator
(`wrangler/src/alert-rules.ts`) through `wrangler/scripts/slow-burn-decide.mjs`;
nothing here reimplements the burn math. The burn ring persists in the
workflow cache between runs; a lost or backwards ring rebuilds instead of
firing, exactly like the Worker restart path.

Usage (run from the repository root):
  STAGING_WORKER_URL=<url> METRICS_SCRAPE_TOKEN=<bearer> RING_PATH=<ring.json> \\
  GITHUB_REPOSITORY=TogetherWeOwn/two-bot-next GH_TOKEN=<token> \\
  python3 scripts/slow_burn_tracker.py --evidence-out tracker-evidence.json

Proof mode (no scrape token, real GitHub writes under a test label):
  python3 scripts/slow_burn_tracker.py --fixture fire --label slow-burn-test
  python3 scripts/slow_burn_tracker.py --fixture clear --label slow-burn-test

Stdlib only. All failures are fixed-vocabulary TrackErrors that never carry
the bearer, the webhook/token value or response bodies.
"""

import argparse
import json
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_DRIVER = REPO_ROOT / "wrangler" / "scripts" / "slow-burn-decide.mjs"
RUNBOOK_URL = (
    "https://github.com/TogetherWeOwn/two-bot-next"
    "/blob/main/docs/runbook.md#alert-voice-join-slow-burn"
)
FIRING_KEY = "voice_join_slow_burn"
TITLE_PREFIX = "[voice-join-slow-burn]"
DEFAULT_LABEL = "slow-burn"
REFRESH_SECONDS = 6 * 3600
# GitHub runs `*/5` schedules on a best-effort cadence: this repo's other
# `*/5` workflow has landed 2-3 h apart. The slow long-window baseline sits
# one long window behind the previous scrape, so once the ring gap reaches
# this threshold the baseline is already pruned and the verdict stays
# unknown. Warn instead of reading green.
CADENCE_WARN_SECONDS = 600
# Entry age alone cannot prove coverage: sub-600 s spacings (420 s steady,
# 590 s steady, alternating 300/560 s) also leave the narrow baseline window
# empty, so the verdict stays unknown with no warning. The tracker therefore
# records the most recent covered moment beside the cached ring and warns
# once the slow windows have gone this long without coverage: genuine burns
# go unfiled and open cards stay unclosed while coverage is gone.
UNCOVERED_WARN_SECONDS = int(6.5 * 3600)
MARKER_RE = re.compile(r"<!-- slow-burn-tracker:last-update=([0-9]{1,19}) -->")
# The staging edge answers the stdlib default agent with 403/error 1010, so
# the scrape names itself (a missing bearer still reads as the Worker's own
# 401, never a 403).
USER_AGENT = "two-bot-next-slow-burn-tracker/1.0"
TIMEOUT_SECONDS = 6
MAX_BODY_BYTES = 64 * 1024
# Plenty for a six-hour ring or 50 ordinary issue rows. Check UTF-8 bytes,
# not characters, before decoding any cached or subprocess JSON.
MAX_JSON_BYTES = 1024 * 1024


# Strings are skipped whole, so brackets inside them never count. An
# unterminated string runs to the end, which keeps the scan linear.
JSON_TOKEN = re.compile(r'"(?:[^"\\]|\\.)*(?:"|\\?\Z)|[\[\]{}]', re.DOTALL)
# CPython's old default recursion limit, which bounded the C JSON decoder.
MAX_JSON_DEPTH = 1000


def _loads_bounded(raw, **kwargs):
    """json.loads with a lexical nesting bound that raises RecursionError.

    Newer CPython sizes the decoder's recursion guard from the real C stack
    and decodes 10,000 levels, so the bound no longer depends on the
    interpreter: existing RecursionError refusals keep working unchanged.
    """
    # Decode bytes exactly as json.loads would, then scan and parse that text.
    if isinstance(raw, (bytes, bytearray)):
        raw = raw.decode(json.detect_encoding(raw), "surrogatepass")
    depth = 0
    for match in JSON_TOKEN.finditer(raw):
        token = match.group()
        if token in "[{":
            depth += 1
            if depth > MAX_JSON_DEPTH:
                raise RecursionError("JSON nesting exceeds the decoder bound")
        elif token in "]}":
            depth -= 1
    return json.loads(raw, **kwargs)


def _load_cache_json(path):
    """Read a bounded cache; missing, oversized or malformed data rebuilds."""
    try:
        with Path(path).open("rb") as stream:
            raw = stream.read(MAX_JSON_BYTES + 1)
        if len(raw) > MAX_JSON_BYTES:
            return None
        return _loads_bounded(raw, object_pairs_hook=_reject_duplicate_keys)
    except (OSError, ValueError, UnicodeError, RecursionError):
        return None


def _intake_json(raw, message):
    """Decode a bounded subprocess answer without exposing remote error text."""
    try:
        if len(raw.encode("utf-8") if isinstance(raw, str) else raw) > MAX_JSON_BYTES:
            raise TrackError(message)
        return _loads_bounded(raw, object_pairs_hook=_reject_duplicate_keys)
    except (ValueError, UnicodeError, RecursionError):
        raise TrackError(message) from None


class TrackError(Exception):
    """A fixed-vocabulary tracker failure (never carries secrets)."""


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


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Turn every redirect into an HTTPError without contacting its target."""

    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


def _utc_now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def scrape_metrics(origin, token):
    """GET {origin}/ops/metrics with the scrape bearer. Returns exposition text."""
    if not origin.startswith("https://"):
        raise TrackError("metrics origin refused: https only")
    url = origin.rstrip("/") + "/ops/metrics"
    request = urllib.request.Request(
        url,
        method="GET",
        headers={"Authorization": f"Bearer {token}", "User-Agent": USER_AGENT},
    )
    opener = urllib.request.build_opener(_NoRedirectHandler())
    try:
        with opener.open(request, timeout=TIMEOUT_SECONDS) as response:
            if response.status != 200:
                raise TrackError(f"metrics scrape refused with status {response.status}")
            body = response.read(MAX_BODY_BYTES + 1)
    except urllib.error.HTTPError as error:
        if error.code in (301, 302, 303, 307, 308):
            raise TrackError(
                f"metrics scrape refused redirect with status {error.code}"
            )
        raise TrackError(f"metrics scrape refused with status {error.code}")
    except urllib.error.URLError:
        raise TrackError("metrics scrape failed: unreachable")
    except (TimeoutError, ConnectionError, OSError):
        raise TrackError("metrics scrape failed: unreachable")
    if len(body) > MAX_BODY_BYTES:
        raise TrackError("metrics scrape failed: body over cap")
    try:
        return body.decode("utf-8")
    except UnicodeDecodeError:
        raise TrackError("metrics scrape failed: body is not text")


def load_ring(path):
    parsed = _load_cache_json(path)
    return parsed if isinstance(parsed, list) else []


def ring_gap_seconds(ring, now):
    """Seconds since the newest ring entry, or None for an empty ring."""
    newest = None
    for entry in ring if isinstance(ring, list) else []:
        tick = entry.get("t") if isinstance(entry, dict) else None
        if isinstance(tick, (int, float)) and tick == tick:
            newest = tick if newest is None else max(newest, tick)
    if newest is None:
        return None
    return now - newest


def _sidecar_tick(parsed, key):
    """One valid positive epoch from the sidecar dict, or None."""
    if not isinstance(parsed, dict):
        return None
    tick = parsed.get(key)
    if isinstance(tick, bool) or not isinstance(tick, (int, float)):
        return None
    if tick != tick or tick <= 0:
        return None
    return int(tick)


def _load_sidecar_dict(path):
    """Sidecar dict, or {} when missing/unreadable/malformed."""
    parsed = _load_cache_json(path)
    if isinstance(parsed, dict):
        return parsed
    if isinstance(parsed, (int, float)) and not isinstance(parsed, bool) \
            and parsed == parsed and parsed > 0:
        return {"last_covered": int(parsed)}
    return {}


def load_covered_at(path):
    """Most recent covered epoch from the coverage sidecar, or None.

    A missing sidecar means coverage was never recorded (fresh install):
    the caller seeds the tracking start instead of staying silent forever.
    """
    return _sidecar_tick(_load_sidecar_dict(path), "last_covered")


def load_tracking_since(path):
    """First live unknown epoch from the coverage sidecar, or None."""
    return _sidecar_tick(_load_sidecar_dict(path), "tracking_since")


def save_covered_at(path, now):
    """Record a covered moment beside the cached ring.

    Preserves an existing tracking start so a later loss still measures
    from the most recent coverage, not from the install day.
    """
    sidecar = _load_sidecar_dict(path)
    sidecar["last_covered"] = int(now)
    Path(path).write_text(json.dumps(sidecar, sort_keys=True) + "\n",
                           encoding="utf-8")


def seed_tracking_since(path, now):
    """Seed the first-unknown moment when the sidecar has neither stamp.

    A tracker that never reaches coverage has no covered moment to measure
    from; without this seed it would stay green-unknown forever. Warm-up
    takes about 6 h, so the 6.5 h warn threshold still leaves a healthy
    install a margin before it can warn.
    """
    sidecar = _load_sidecar_dict(path)
    if _sidecar_tick(sidecar, "last_covered") is not None:
        return sidecar
    if _sidecar_tick(sidecar, "tracking_since") is not None:
        return sidecar
    sidecar["tracking_since"] = int(now)
    Path(path).write_text(json.dumps(sidecar, sort_keys=True) + "\n",
                           encoding="utf-8")
    return sidecar


def run_driver(node, driver, exposition_path, ring_path, now):
    """Ask the checked-in evaluator for the slow-burn verdict. Returns the verdict dict."""
    try:
        completed = subprocess.run(
            [node, str(driver), "--exposition", str(exposition_path),
             "--ring", str(ring_path), "--now", str(now)],
            capture_output=True, timeout=120, check=False,
        )
    except (OSError, subprocess.SubprocessError):
        raise TrackError("burn evaluation failed: driver did not run")
    if completed.returncode != 0:
        raise TrackError("burn evaluation failed: driver refused its inputs")
    verdict = _intake_json(
        completed.stdout, "burn evaluation failed: driver answer is not bounded JSON")
    if (not isinstance(verdict, dict) or not isinstance(verdict.get("slow"), bool)
            or not isinstance(verdict.get("held"), bool)
            or not isinstance(verdict.get("slowCovered"), bool)
            or not isinstance(verdict.get("ring"), list)):
        raise TrackError("burn evaluation failed: driver answer has no verdict")
    return verdict


def fixture_inputs(kind, now):
    """Now-anchored (exposition, ring) fixture. `fire` burns at 8x, `clear` is healthy."""
    if kind not in ("fire", "clear"):
        raise TrackError("fixture refused: expected fire or clear")
    # Five-minute steps at 8x burn (25 genuine, 1 slow each): all but the
    # last form the restored ring and the last is the fresh scrape, because
    # the evaluator lags receive counters one scrape behind moves. A ring
    # that already contains the exposition has an empty common window and
    # stays silent by design. The five-minute cadence is load-bearing: the
    # slow long-window baseline sits one long window behind the previous
    # scrape, so anything coarser than ten minutes prunes its own baseline
    # and the ticket can never fire.
    step = 300
    snapshots = 84
    ring = []
    received = 0
    fast = 0
    for index in range(snapshots + 1):
        received += 25
        fast += 24 if kind == "fire" else 25
        if index < snapshots:
            ring.append({
                "t": now - (snapshots - index) * step,
                "received": received,
                "duplicate": 0,
                "fastMoves": fast,
            })
    # Checked-in complete companions keep proof mode under the same strict
    # scrape contract as live mode. The driver never fills missing live series.
    try:
        companions = (REPO_ROOT / "scripts" / "fixtures" / "slow-burn-companion.prom").read_text(
            encoding="utf-8")
    except (OSError, UnicodeError):
        raise TrackError("fixture refused: companion metrics are unreadable") from None
    exposition = companions + (
        f"two_bot_voice_joins_received_total {received}\n"
        f'two_bot_voice_join_refused_total{{reason="duplicate"}} 0\n'
        f'two_bot_voice_join_to_move_seconds_bucket{{le="5"}} {fast}\n'
    )
    return exposition, ring


def gh(args, repo, token_env):
    """Run one `gh issue` command. Returns stdout text."""
    try:
        completed = subprocess.run(
            ["gh", "issue"] + args + ["--repo", repo],
            capture_output=True, text=True, timeout=60, check=False,
            env={"GH_TOKEN": token_env, "PATH": "/usr/bin:/bin:/usr/local/bin"},
        )
    except UnicodeDecodeError:
        raise TrackError("issue reconcile failed: gh answer is not text")
    except (OSError, subprocess.SubprocessError):
        raise TrackError("issue reconcile failed: gh did not run")
    if completed.returncode != 0:
        raise TrackError("issue reconcile failed: gh refused the call")
    return completed.stdout


def gh_with_body(args, body, repo, token_env):
    """Run one `gh issue` command whose `--body-file` lives in private scratch.

    The body file never touches the working directory, so a run (or a test)
    from a checkout leaves the tree clean.
    """
    try:
        scratch = tempfile.TemporaryDirectory(prefix="slow-burn-gh-body-")
    except OSError:
        raise TrackError("issue reconcile failed: body scratch I/O failed")
    try:
        body_path = Path(scratch.name) / "body.md"
        try:
            body_path.write_text(body, encoding="utf-8")
        except OSError:
            raise TrackError("issue reconcile failed: body scratch I/O failed")
        return gh(args + ["--body-file", str(body_path)], repo, token_env)
    finally:
        try:
            scratch.cleanup()
        except OSError:
            # gh may already have accepted the write; a leftover scratch
            # directory must not turn that into a failure with no receipt.
            pass


def open_tracking_issues(repo, token, label, prefix):
    try:
        raw = gh(["list", "--state", "open", "--label", label,
                  "--json", "number,title,body", "--limit", "50"],
                 repo, token)
    except TrackError:
        raise TrackError("issue reconcile failed: open issues are unreadable")
    issues = _intake_json(raw, "issue reconcile failed: open issues are unreadable")
    if not isinstance(issues, list) or any(
            not isinstance(issue, dict)
            or not isinstance(issue.get("title"), str)
            or type(issue.get("number")) is not int
            or issue["number"] <= 0
            or "body" not in issue
            or (issue["body"] is not None and not isinstance(issue["body"], str))
            for issue in issues):
        raise TrackError("issue reconcile failed: open issues are unreadable")
    return [issue for issue in issues if issue["title"].startswith(prefix)]


def marker_time(body):
    if not isinstance(body, str):
        return 0
    match = MARKER_RE.search(body)
    return int(match.group(1)) if match else 0


def with_marker(body, now):
    line = f"<!-- slow-burn-tracker:last-update={now} -->"
    if MARKER_RE.search(body or ""):
        return MARKER_RE.sub(line, body)
    return (body.rstrip() + "\n\n" + line + "\n") if body else (line + "\n")


def issue_title(prefix):
    return f"{prefix} creator-join error budget in sustained slow burn"


def issue_body(origin, run_url, now, fast_also):
    lines = [
        f"{FIRING_KEY} is firing: genuine creator joins are burning the "
        "99.5%-moved-within-5 s error budget faster than 6x in both the last "
        "6 hours and the last 30 minutes.",
        "",
        "First response: follow the slow-burn runbook section: "
        f"{RUNBOOK_URL}. "
        "Do not close this as fast-burn-resolved: the fast window can clear "
        "while the slow grind continues.",
    ]
    if fast_also:
        lines.append("The fast-burn page is firing too: the on-call page is "
                     "already out for the same budget.")
    lines += [
        "",
        f"Detected from {origin} at { _utc_now() }.",
        f"Tracker run: {run_url}",
        "",
        f"<!-- slow-burn-tracker:last-update={now} -->",
    ]
    return "\n".join(lines) + "\n"


def resolve_comment(run_url):
    return (
        f"{FIRING_KEY} cleared: the slow-burn windows no longer burn above "
        f"the ticket threshold. Tracker run: {run_url}\n"
    )


def refresh_comment(run_url):
    return (
        f"{FIRING_KEY} still firing as of { _utc_now() }: the sustained grind "
        f"continues. Tracker run: {run_url}\n"
    )


def reconcile(*, repo, token, label, prefix, origin, run_url, now,
              slow, fast_also, dry_run):
    """File, refresh or resolve the tracking issue. Returns (action, issue_number|None)."""
    tracked = open_tracking_issues(repo, token, label, prefix)
    if slow:
        if not tracked:
            title = issue_title(prefix)
            if dry_run:
                print(f"dry-run: would create issue {title!r} with label {label!r}")
                return "would-create", None
            out = gh_with_body(["create", "--title", title, "--label", label],
                               issue_body(origin, run_url, now, fast_also),
                               repo, token)
            match = re.search(r"/issues/(\d+)\s*$", out.strip())
            number = int(match.group(1)) if match else None
            return "created", number
        issue = sorted(tracked, key=lambda item: item.get("number", 0))[0]
        number = issue["number"]
        if now - marker_time(issue.get("body")) < REFRESH_SECONDS:
            return "already-open", number
        if dry_run:
            print(f"dry-run: would refresh comment on issue #{number}")
            return "would-refresh", number
        gh_with_body(["comment", str(number)], refresh_comment(run_url),
                     repo, token)
        gh_with_body(["edit", str(number)],
                     with_marker(issue.get("body") or "", now), repo, token)
        return "refreshed", number
    if not tracked:
        return "none-open", None
    issue = sorted(tracked, key=lambda item: item.get("number", 0))[0]
    number = issue["number"]
    if dry_run:
        print(f"dry-run: would resolve and close issue #{number}")
        return "would-close", number
    gh_with_body(["comment", str(number)], resolve_comment(run_url), repo, token)
    gh(["close", str(number), "--reason", "completed"], repo, token)
    return "closed", number


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ring", default="slow-burn-ring.json",
                        help="persisted burn ring path (workflow cache)")
    parser.add_argument("--coverage-path", default="slow-burn-coverage.json",
                        help="most-recent-covered sidecar path (workflow cache)")
    parser.add_argument("--evidence-out", default="slow-burn-tracker-evidence.json",
                        help="evidence receipt path for the workflow artifact")
    parser.add_argument("--exposition-out", default="slow-burn-exposition.prom",
                        help="scraped exposition path for the workflow artifact")
    parser.add_argument("--fixture", choices=("live", "fire", "clear"), default="live",
                        help="live scrapes staging; fire/clear use a now-anchored fixture")
    parser.add_argument("--label", default=DEFAULT_LABEL,
                        help="tracking issue label (proof runs use a test label)")
    parser.add_argument("--title-prefix", default=TITLE_PREFIX,
                        help="tracking issue title prefix (dedup key)")
    parser.add_argument("--dry-run", action="store_true",
                        help="decide and print, never call gh for writes")
    parser.add_argument("--node", default="node", help="node binary for the driver")
    parser.add_argument("--driver", default=str(DEFAULT_DRIVER),
                        help="slow-burn-decide.mjs path")
    return parser.parse_args(argv)


def main(argv=None, env=None):
    # argv=None reads sys.argv: never substitute an empty list, or the
    # workflow's --fixture/--label/--dry-run flags are silently dropped.
    args = parse_args(argv)
    env = env if env is not None else dict(__import__("os").environ)
    now = int(time.time())
    repo = env.get("GITHUB_REPOSITORY", "")
    run_url = "/".join([
        env.get("GITHUB_SERVER_URL", "https://github.com").rstrip("/"),
        repo, "actions", "runs", env.get("GITHUB_RUN_ID", "0"),
    ])
    evidence = {"utc": _utc_now(), "origin": "", "mode": args.fixture,
                "verdict": "unknown", "ring_size": 0,
                "action": "none", "issue": None, "run_url": run_url,
                "stale_gap_seconds": None, "uncovered_seconds": None,
                "cadence": "ok"}
    cadence_gap = None

    if args.fixture == "live":
        origin = env.get("STAGING_WORKER_URL", "")
        token = env.get("METRICS_SCRAPE_TOKEN", "")
        if not origin or not token:
            missing = ", ".join(name for name, value in
                                 (("STAGING_WORKER_URL", origin),
                                  ("METRICS_SCRAPE_TOKEN", token)) if not value)
            print(f"::error::slow-burn tracker misconfigured: {missing} unset; "
                  "see the slow-burn runbook section")
            evidence["verdict"] = "unconfigured"
            Path(args.evidence_out).write_text(json.dumps(evidence, indent=2) + "\n",
                                               encoding="utf-8")
            return 1
        evidence["origin"] = origin
        try:
            exposition = scrape_metrics(origin, token)
        except TrackError as error:
            print(f"::error::{error}")
            Path(args.evidence_out).write_text(json.dumps(evidence, indent=2) + "\n",
                                               encoding="utf-8")
            return 1
        Path(args.exposition_out).write_text(exposition, encoding="utf-8")
        ring = load_ring(args.ring)
        cadence_gap = ring_gap_seconds(ring, now)
        if cadence_gap is not None:
            evidence["stale_gap_seconds"] = cadence_gap
            if cadence_gap >= CADENCE_WARN_SECONDS:
                evidence["cadence"] = "coarse"
                print(f"::warning::slow-burn tracker cadence is coarse "
                      f"(newest ring entry {int(cadence_gap)}s old, "
                      f"threshold {CADENCE_WARN_SECONDS}s); the 6h baseline "
                      f"is pruned and the verdict may stay unknown until "
                      f"the 5-minute schedule catches up")
    else:
        evidence["origin"] = f"fixture:{args.fixture}"
        try:
            exposition, ring = fixture_inputs(args.fixture, now)
        except TrackError as error:
            print(f"::error::{error}")
            Path(args.evidence_out).write_text(json.dumps(evidence, indent=2) + "\n",
                                               encoding="utf-8")
            return 1
        Path(args.exposition_out).write_text(exposition, encoding="utf-8")

    gh_token = env.get("GH_TOKEN", "")
    if not repo or not gh_token:
        print("::error::slow-burn tracker misconfigured: "
              "GITHUB_REPOSITORY or GH_TOKEN unset")
        return 1

    exposition_path = Path(args.exposition_out)
    try:
        with tempfile.TemporaryDirectory(
                prefix="slow-burn-ring-", dir=exposition_path.resolve().parent) as scratch:
            ring_path = Path(scratch) / "prev.json"
            ring_path.write_text(json.dumps(ring) + "\n", encoding="utf-8")
            verdict = run_driver(args.node, args.driver, exposition_path, ring_path, now)
    except (TrackError, OSError) as error:
        if isinstance(error, OSError):
            error = TrackError("burn evaluation failed: ring scratch I/O failed")
        print(f"::error::{error}")
        Path(args.evidence_out).write_text(json.dumps(evidence, indent=2) + "\n",
                                           encoding="utf-8")
        return 1
    next_ring = verdict["ring"]
    evidence["ring_size"] = len(next_ring)
    if verdict["held"]:
        # The join series were unusable: the previous firing state holds, so
        # this run decides nothing. It must not file, refresh, resolve, close
        # or rewrite the ring; silence here is not recovery.
        evidence["verdict"] = "held"
        Path(args.evidence_out).write_text(json.dumps(evidence, indent=2) + "\n",
                                           encoding="utf-8")
        print(f"slow-burn tracker: verdict=held action=none ring={len(next_ring)}")
        return 0
    slow_covered = verdict["slowCovered"]
    if verdict["slow"]:
        evidence["verdict"] = "slow-fire"
    elif slow_covered:
        evidence["verdict"] = "clear"
    else:
        # The slow windows lack full history (late start, a coarse cadence,
        # or a rebuilt ring): this is unknown, not a clear. The ring still
        # persists so history rebuilds, but no issue is filed, refreshed or
        # closed; silence here is not recovery.
        evidence["verdict"] = "unknown"
        if args.fixture == "live":
            Path(args.ring).write_text(json.dumps(next_ring) + "\n", encoding="utf-8")
            covered_at = load_covered_at(args.coverage_path)
            tracking_since = load_tracking_since(args.coverage_path)
            if covered_at is None and tracking_since is None:
                seed_tracking_since(args.coverage_path, now)
                tracking_since = now
            reference = covered_at if covered_at is not None else tracking_since
            if covered_at is not None and tracking_since is not None:
                reference = max(covered_at, tracking_since)
            uncovered = max(0, now - reference)
            evidence["uncovered_seconds"] = uncovered
            if uncovered >= UNCOVERED_WARN_SECONDS:
                evidence["cadence"] = "coarse"
                print(f"::warning::slow-burn tracker has no covered "
                      f"slow-window verdict for {int(uncovered)}s "
                      f"(threshold {UNCOVERED_WARN_SECONDS}s); genuine "
                      f"burns go unfiled and open cards stay unclosed "
                      f"until coverage returns")
        Path(args.evidence_out).write_text(json.dumps(evidence, indent=2) + "\n",
                                           encoding="utf-8")
        print(f"slow-burn tracker: verdict=unknown action=none ring={len(next_ring)}")
        return 0
    if args.fixture == "live":
        Path(args.ring).write_text(json.dumps(next_ring) + "\n", encoding="utf-8")
        save_covered_at(args.coverage_path, now)

    try:
        action, number = reconcile(
            repo=repo, token=gh_token, label=args.label,
            prefix=args.title_prefix, origin=evidence["origin"],
            run_url=run_url, now=now, slow=verdict["slow"],
            fast_also=bool(verdict.get("fast")), dry_run=args.dry_run)
    except TrackError as error:
        print(f"::error::{error}")
        Path(args.evidence_out).write_text(json.dumps(evidence, indent=2) + "\n",
                                           encoding="utf-8")
        return 1
    evidence["action"] = action
    evidence["issue"] = number
    Path(args.evidence_out).write_text(json.dumps(evidence, indent=2) + "\n",
                                       encoding="utf-8")
    print(f"slow-burn tracker: verdict={evidence['verdict']} action={action} "
          f"issue={number} ring={len(next_ring)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
