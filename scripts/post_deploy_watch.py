#!/usr/bin/env python3
"""Watch production for 30 minutes after a promotion; report a breach.

Runs in the deploy-production `post-deploy-watch` job, after the `production`
job certified the new build. The in-run gates judge a few minutes of
readiness and one REST 5xx window; this watch keeps judging the same signals
for the rest of the window and reports one verdict:

  clean       30 minutes with no breach.
  superseded  the promoted Worker version no longer serves 100% (a newer
              deploy or an operator rollback moved traffic): the watch stops
              without touching anything.
  breach      either
                - readiness: MISS_LIMIT consecutive polls (~3 min) where
                  /health is not 200, or /readyz is not 200 with gateway
                  ready and the promoted build revision; or
                - rest_5xx: a promotion error-rate gate window
                  (wrangler/scripts/promotion-error-gate.mjs, the same
                  evaluatePromotionGate() verdict as the deploy gate) fails,
                  or two windows in a row cannot scrape.
              The workflow then restores the pre-deploy snapshot (Worker
              version AND container image) with the reviewed rollback script.
  error       the watch itself could not read Cloudflare: no rollback, the
              job fails red for a human.

The verdict goes to GITHUB_OUTPUT (`verdict`, `reason`) and the evidence file.
Evidence carries counts, fixed reason codes and IDs only: never remote
bodies, headers or credentials. Exit 0 for clean or superseded, 1 for a
breach, 3 for a watch error.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
# Same E402-safe convention as the other scripts/ tools.
import staging_rollout as base  # noqa: E402
import worker_container_rollback as wcr  # noqa: E402
from production_deploy import MAX_READYZ_BYTES, _gateway_ready  # noqa: E402

GateError = base.GateError
WORKER = wcr.WORKERS["production"]
USER_AGENT = "two-bot-next-production-rollout/1.0"
DURATION_SECONDS = 1800
POLL_SECONDS = 10
MISS_LIMIT = 18  # consecutive missed polls (~3 min), the deploy gate's stability window
VERSION_EVERY_SECONDS = 60
GATE_WINDOW_SECONDS = 300
GATE_SAMPLE_SECONDS = 240  # two scrapes inside each window, with slack for retries
GATE_ERROR_LIMIT = 2
SHA = re.compile(r"[0-9a-f]{40}")


def fetch(url):
    """(status, body) for one GET; 0 on transport failure. Never raises."""
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT, "Cache-Control": "no-cache"})
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, response.read(MAX_READYZ_BYTES + 1)
    except urllib.error.HTTPError as error:
        try:
            return error.code, error.read(MAX_READYZ_BYTES + 1)
        except OSError:
            return error.code, b""
    except (OSError, ValueError):
        return 0, b""


def ready(url, sha, fetch_fn=fetch):
    """(passed, reason code) for one /health + /readyz poll."""
    status, _ = fetch_fn(url + "/health")
    if status != 200:
        return False, "health_not_200"
    status, body = fetch_fn(url + "/readyz")
    if status != 200:
        return False, "readyz_not_200"
    if len(body) > MAX_READYZ_BYTES:
        return False, "readyz_over_cap"
    try:
        report = json.loads(body.decode("utf-8"), object_pairs_hook=wcr._reject_duplicate_keys)
    except (ValueError, UnicodeDecodeError, RecursionError):
        return False, "readyz_not_json"
    if not isinstance(report, dict) or not _gateway_ready(report):
        return False, "gateway_not_ready"
    if report.get("build_revision") != sha:
        return False, "revision_mismatch"
    return True, "ready"


class Gate:
    """One promotion error-rate gate window at a time, as a child process."""

    def __init__(self, url, sha, workdir, tooling):
        self.url, self.sha, self.workdir, self.tooling = url, sha, Path(workdir), Path(tooling)
        self.child, self.index, self.errors = None, 0, 0

    def start(self):
        self.index += 1
        self.evidence = self.workdir / f"watch-error-rate-{self.index}.json"
        self.child = subprocess.Popen(
            ["node", str(self.tooling / "wrangler/scripts/promotion-error-gate.mjs"),
             "--metrics-url", self.url, "--evidence", str(self.evidence), "--sha", self.sha,
             "--sample-interval-seconds", str(GATE_SAMPLE_SECONDS), "--user-agent", USER_AGENT],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def poll(self):
        """None while running; otherwise the window verdict (pass/fail/skip/insufficient_data/error)."""
        if self.child is None or self.child.poll() is None:
            return None
        self.child = None
        try:
            verdict = json.loads(self.evidence.read_text()).get("verdict")
        except (OSError, ValueError, AttributeError):
            verdict = "error"
        return verdict if verdict in ("pass", "fail", "skip", "insufficient_data") else "error"

    def stop(self):
        if self.child is not None and self.child.poll() is None:
            self.child.kill()
            self.child.wait()


def watch(*, url, sha, version, client, gate, now=time.monotonic, sleep=time.sleep,
          fetch_fn=fetch, duration=DURATION_SECONDS):
    """The watch verdict and its evidence."""
    started = now()
    evidence = {"sha": sha, "version": version, "polls": 0, "misses": 0, "max_miss_streak": 0,
                "gate_windows": [], "verdict": "clean", "reason": "no_breach"}
    streak, last_version_check, gate_started = 0, None, None

    def finish(verdict, reason):
        gate.stop()
        evidence.update(verdict=verdict, reason=reason, watched_seconds=int(now() - started))
        return evidence

    while now() - started < duration:
        moment = now()
        if last_version_check is None or moment - last_version_check >= VERSION_EVERY_SECONDS:
            last_version_check = moment
            try:
                serving = wcr.deployments(client, WORKER)[0]
            except GateError:
                return finish("error", "cloudflare_unreadable")
            if serving != version:
                return finish("superseded", "version_no_longer_serving")
        passed, reason = ready(url, sha, fetch_fn)
        evidence["polls"] += 1
        if passed:
            streak = 0
        else:
            streak += 1
            evidence["misses"] += 1
            evidence["max_miss_streak"] = max(evidence["max_miss_streak"], streak)
            if streak >= MISS_LIMIT:
                return finish("breach", f"readiness:{reason}")
        if gate_started is None and duration - (moment - started) >= GATE_WINDOW_SECONDS:
            gate.start()
            gate_started = moment
        verdict = gate.poll() if gate_started is not None else None
        if verdict is not None:
            gate_started = None
            evidence["gate_windows"].append(verdict)
            if verdict == "fail":
                return finish("breach", "rest_5xx")
            gate.errors = gate.errors + 1 if verdict == "error" else 0
            if gate.errors >= GATE_ERROR_LIMIT:
                return finish("breach", "rest_5xx_unreadable")
        sleep(POLL_SECONDS)
    return finish("clean", "no_breach")


def write_github(name, text):
    path = os.environ.get(name)
    if path:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(text)


def main(argv=None, *, client_factory=wcr.RollbackClient, gate_factory=Gate, **watch_kwargs):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--worker-url", required=True)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--version", required=True, help="promoted Worker version ID")
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--tooling", default=str(Path(__file__).resolve().parents[1]))
    args = parser.parse_args(argv)
    url = args.worker_url.strip().rstrip("/")
    if not url.startswith("https://") or not SHA.fullmatch(args.sha) \
            or not wcr.VERSION_ID.fullmatch(args.version):
        print("::error::post-deploy watch needs the production https origin, the guarded SHA "
              "and the promoted Worker version")
        return 3
    try:
        client = client_factory(os.environ.get("CLOUDFLARE_ACCOUNT_ID"), os.environ.get("CLOUDFLARE_API_TOKEN"))
    except GateError as error:
        evidence = {"sha": args.sha, "version": args.version, "verdict": "error", "reason": str(error)}
    else:
        gate = gate_factory(url, args.sha, Path(args.evidence).parent, args.tooling)
        evidence = watch(url=url, sha=args.sha, version=args.version, client=client, gate=gate,
                         **watch_kwargs)
    Path(args.evidence).write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    print("post-deploy watch: " + json.dumps(evidence, sort_keys=True))
    write_github("GITHUB_OUTPUT", f"verdict={evidence['verdict']}\nreason={evidence['reason']}\n")
    write_github("GITHUB_STEP_SUMMARY",
                 f"- Post-deploy watch: `{evidence['verdict']}` ({evidence['reason']}); "
                 f"{evidence.get('polls', 0)} polls, max miss streak {evidence.get('max_miss_streak', 0)}, "
                 f"error-rate windows {', '.join(evidence.get('gate_windows', [])) or 'none'}\n")
    if evidence["verdict"] == "breach":
        print(f"::error::post-deploy watch breach ({evidence['reason']}): rolling back")
        return 1
    if evidence["verdict"] == "error":
        print(f"::error::post-deploy watch could not judge ({evidence['reason']}); no rollback, check production")
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
