#!/usr/bin/env python3
"""File, refresh or resolve the creator-join slow-burn tracking issue.

The `voice_join_slow_burn` Worker rule only alerts: this scheduled tracker
turns the first firing into one GitHub tracking issue and closes it when the
burn clears. While the burn keeps firing the open issue is left alone (a
refresh comment lands at most every six hours), so a refire never duplicates
the card. An unknown verdict (failed scrape, failed evaluation, or unusable
join series) never closes an open card: silence is not recovery.

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
MARKER_RE = re.compile(r"<!-- slow-burn-tracker:last-update=(\d+) -->")
TIMEOUT_SECONDS = 6
MAX_BODY_BYTES = 64 * 1024


class TrackError(Exception):
    """A fixed-vocabulary tracker failure (never carries secrets)."""


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
        url, method="GET", headers={"Authorization": f"Bearer {token}"}
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
    try:
        raw = Path(path).read_text(encoding="utf-8")
    except OSError:
        return []
    try:
        parsed = json.loads(raw)
    except ValueError:
        return []
    return parsed if isinstance(parsed, list) else []


def run_driver(node, driver, exposition_path, ring_path, now):
    """Ask the checked-in evaluator for the slow-burn verdict. Returns the verdict dict."""
    try:
        completed = subprocess.run(
            [node, str(driver), "--exposition", str(exposition_path),
             "--ring", str(ring_path), "--now", str(now)],
            capture_output=True, text=True, timeout=120, check=False,
        )
    except (OSError, subprocess.SubprocessError):
        raise TrackError("burn evaluation failed: driver did not run")
    if completed.returncode != 0:
        raise TrackError("burn evaluation failed: driver refused its inputs")
    try:
        verdict = json.loads(completed.stdout)
    except ValueError:
        raise TrackError("burn evaluation failed: driver answer is not JSON")
    if (not isinstance(verdict, dict) or not isinstance(verdict.get("slow"), bool)
            or not isinstance(verdict.get("held"), bool)
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
    exposition = (
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
    except (OSError, subprocess.SubprocessError):
        raise TrackError("issue reconcile failed: gh did not run")
    if completed.returncode != 0:
        raise TrackError("issue reconcile failed: gh refused the call")
    return completed.stdout


def open_tracking_issues(repo, token, label, prefix):
    try:
        raw = gh(["list", "--state", "open", "--label", label,
                  "--json", "number,title,body", "--limit", "50"],
                 repo, token)
    except TrackError:
        raise TrackError("issue reconcile failed: open issues are unreadable")
    try:
        issues = json.loads(raw)
    except ValueError:
        raise TrackError("issue reconcile failed: open issues are unreadable")
    if not isinstance(issues, list):
        raise TrackError("issue reconcile failed: open issues are unreadable")
    return [issue for issue in issues
            if isinstance(issue, dict)
            and isinstance(issue.get("title"), str)
            and issue["title"].startswith(prefix)]


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
            body_path = Path("tracker-issue-body.md")
            body_path.write_text(
                issue_body(origin, run_url, now, fast_also), encoding="utf-8")
            out = gh(["create", "--title", title, "--body-file", str(body_path),
                      "--label", label], repo, token)
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
        comment_path = Path("tracker-refresh-body.md")
        comment_path.write_text(refresh_comment(run_url), encoding="utf-8")
        gh(["comment", str(number), "--body-file", str(comment_path)], repo, token)
        body_path = Path("tracker-issue-body.md")
        body_path.write_text(
            with_marker(issue.get("body") or "", now), encoding="utf-8")
        gh(["edit", str(number), "--body-file", str(body_path)], repo, token)
        return "refreshed", number
    if not tracked:
        return "none-open", None
    issue = sorted(tracked, key=lambda item: item.get("number", 0))[0]
    number = issue["number"]
    if dry_run:
        print(f"dry-run: would resolve and close issue #{number}")
        return "would-close", number
    comment_path = Path("tracker-resolve-body.md")
    comment_path.write_text(resolve_comment(run_url), encoding="utf-8")
    gh(["comment", str(number), "--body-file", str(comment_path)], repo, token)
    gh(["close", str(number), "--reason", "completed"], repo, token)
    return "closed", number


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ring", default="slow-burn-ring.json",
                        help="persisted burn ring path (workflow cache)")
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
    args = parse_args(argv or [])
    env = env if env is not None else dict(__import__("os").environ)
    now = int(time.time())
    repo = env.get("GITHUB_REPOSITORY", "")
    run_url = "/".join([
        env.get("GITHUB_SERVER_URL", "https://github.com").rstrip("/"),
        repo, "actions", "runs", env.get("GITHUB_RUN_ID", "0"),
    ])
    evidence = {"utc": _utc_now(), "origin": "", "mode": args.fixture,
                "verdict": "unknown", "ring_size": 0,
                "action": "none", "issue": None, "run_url": run_url}

    if args.fixture == "live":
        origin = env.get("STAGING_WORKER_URL", "")
        token = env.get("METRICS_SCRAPE_TOKEN", "")
        if not origin or not token:
            print("::warning::slow-burn tracker not configured yet "
                  "(STAGING_WORKER_URL or METRICS_SCRAPE_TOKEN unset); "
                  "see the slow-burn runbook section")
            evidence["verdict"] = "unconfigured"
            Path(args.evidence_out).write_text(json.dumps(evidence, indent=2) + "\n",
                                               encoding="utf-8")
            return 0
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
    else:
        evidence["origin"] = f"fixture:{args.fixture}"
        exposition, ring = fixture_inputs(args.fixture, now)
        Path(args.exposition_out).write_text(exposition, encoding="utf-8")

    gh_token = env.get("GH_TOKEN", "")
    if not repo or not gh_token:
        print("::error::slow-burn tracker misconfigured: "
              "GITHUB_REPOSITORY or GH_TOKEN unset")
        return 1

    exposition_path = Path(args.exposition_out)
    ring_path = Path("slow-burn-ring-prev.json")
    ring_path.write_text(json.dumps(ring) + "\n", encoding="utf-8")
    try:
        verdict = run_driver(args.node, args.driver, exposition_path, ring_path, now)
    except TrackError as error:
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
    evidence["verdict"] = "slow-fire" if verdict["slow"] else "clear"
    if args.fixture == "live":
        Path(args.ring).write_text(json.dumps(next_ring) + "\n", encoding="utf-8")

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
