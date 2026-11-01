#!/usr/bin/env python3
"""Report a production deploy or rollback to the release-verification webhook.

The last step of the deploy-production `production` job (and any rollback)
POSTs one signed JSON record so the release log hears about every
production change without anyone watching Actions:

  {"repo": "two-bot-next", "event": "deploy"|"rollback",
   "outcome": "success"|"rolled_back"|"failed", "sha", "versionId",
   "imageDigest", "previousVersionId", "runUrl", "startedAt", "finishedAt",
   "reason"}

Signing: headers `X-Paperclip-Timestamp: <unix seconds>` and
`X-Paperclip-Signature: sha256=<hex>`, where hex is
HMAC-SHA256(secret, "<timestamp>." + exact body bytes). The receiver keeps a
300 s replay window, so the body is signed immediately before sending.

Bindings: repository variable PAPERCLIP_DEPLOY_WEBHOOK_URL (https only) and
secret PAPERCLIP_DEPLOY_WEBHOOK_SECRET, both read from the environment.
Without either the step records a notice and exits 0. A notification never
gates a deploy or a rollback: the workflow step is non-blocking, and the
daily release audit cross-checks GitHub runs, so a missed delivery is still
caught. Nothing here prints the secret, the signature or the response body.
"""

import argparse
import hashlib
import hmac
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

REPO = "two-bot-next"
EVENTS = ("deploy", "rollback")
OUTCOMES = ("success", "rolled_back", "failed")
SHA = re.compile(r"[0-9a-f]{40}")
VERSION = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
IMAGE = re.compile(r"registry\.cloudflare\.com/[^/:@\s]+/[^/:@\s]+@sha256:[0-9a-f]{64}")
RUN_URL = re.compile(r"https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/actions/runs/[0-9]+")
TIMESTAMP = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z")
REASON = re.compile(r"[A-Za-z0-9 _:;,./()-]{1,200}")
TIMEOUT_SECONDS = 15


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def only(pattern, value):
    """The value when it matches the allowlisted shape exactly, else None."""
    return value if isinstance(value, str) and pattern.fullmatch(value) else None


def payload(args, finished_at):
    if args.event not in EVENTS or args.outcome not in OUTCOMES:
        raise ValueError("event and outcome must use the fixed vocabulary")
    return {
        "repo": REPO,
        "event": args.event,
        "outcome": args.outcome,
        "sha": only(SHA, args.sha),
        "versionId": only(VERSION, args.version),
        "imageDigest": only(IMAGE, args.image),
        "previousVersionId": only(VERSION, args.previous_version),
        "runUrl": only(RUN_URL, args.run_url),
        "startedAt": only(TIMESTAMP, args.started_at),
        "finishedAt": finished_at,
        "reason": only(REASON, args.reason),
    }


# The production job's gates in run order: the first failed one names a rollback.
GATES = (("GATE_OUTCOME", "readyz gate"),
         ("PROMOTION_OUTCOME", "promoted image recheck"),
         ("ERROR_GATE_OUTCOME", "REST 5xx error-rate gate"))


def read_json(path):
    try:
        with open(path, encoding="utf-8") as handle:
            value = json.load(handle)
    except (OSError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def from_workflow(env):
    """(event, outcome, version, image, previous version, reason) from the production job."""
    temp = env.get("RUNNER_TEMP", "")
    failed = next((name for key, name in GATES if env.get(key) == "failure"), None)
    if env.get("MODE") == "rollback":
        ok = env.get("ROLLBACK_OUTCOME") == "success" and env.get("JOB_STATUS") == "success"
        return ("rollback", "success" if ok else "failed", env.get("TARGET_VERSION", ""),
                env.get("TARGET_IMAGE", ""), "",
                "operator rollback dispatch" + ("" if ok else f"; failed at {failed or 'the restore'}"))
    snapshot = read_json(os.path.join(temp, "production-rollback-snapshot.json"))
    previous = snapshot.get("serving_version", "") if snapshot.get("has_previous") is True else ""
    image = read_json(os.path.join(temp, "production-promotion-evidence.json")).get("production_image", "")
    version = env.get("NEW_VERSION", "")
    if env.get("WATCH_ROLLBACK_OUTCOME") == "success":
        # The post-deploy watch restored the snapshot after the gates passed.
        return ("deploy", "rolled_back", version, image, previous,
                f"auto-rollback after the post-deploy watch breached ({env.get('WATCH_REASON') or 'unknown'})")
    if env.get("AUTO_ROLLBACK_OUTCOME") == "success":
        return ("deploy", "rolled_back", version, image, previous,
                f"auto-rollback after the {failed or 'post-deploy checks'} failed")
    if env.get("JOB_STATUS") == "success":
        return "deploy", "success", version, image, previous, ""
    return ("deploy", "failed", version, image, previous,
            f"{failed or 'a step after the deploy'} failed; no automatic rollback ran")


def started_at(env):
    """The run's start time from the GitHub API, or "" when it cannot be read."""
    repo, run_id = env.get("GITHUB_REPOSITORY", ""), env.get("GITHUB_RUN_ID", "")
    if not repo or not run_id.isdigit():
        return ""
    try:
        return subprocess.run(
            ["gh", "api", f"repos/{repo}/actions/runs/{run_id}", "--jq", ".run_started_at"],
            check=True, capture_output=True, text=True, timeout=20).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def sign(secret, timestamp, body):
    digest = hmac.new(secret.encode(), f"{timestamp}.".encode() + body, hashlib.sha256).hexdigest()
    return f"sha256={digest}"


def send(url, secret, record, *, now=time.time, opener=None):
    """POST the signed record; the HTTP status, or 0 on a transport failure."""
    body = json.dumps(record, separators=(",", ":"), sort_keys=True).encode()
    timestamp = str(int(now()))
    request = urllib.request.Request(url, data=body, method="POST", headers={
        "Content-Type": "application/json",
        "User-Agent": "two-bot-next-deploy-notify/1.0",
        "X-Paperclip-Timestamp": timestamp,
        "X-Paperclip-Signature": sign(secret, timestamp, body),
    })
    opener = opener or urllib.request.build_opener(NoRedirect())
    try:
        with opener.open(request, timeout=TIMEOUT_SECONDS) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code
    except (OSError, ValueError):
        return 0


def main(argv=None, *, environ=None, now=time.time, opener=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--from-workflow", action="store_true",
                        help="derive event, outcome, IDs and reason from the production job")
    parser.add_argument("--event", choices=EVENTS)
    parser.add_argument("--outcome", choices=OUTCOMES)
    parser.add_argument("--sha", default="")
    parser.add_argument("--version", default="")
    parser.add_argument("--image", default="")
    parser.add_argument("--previous-version", default="")
    parser.add_argument("--run-url", default="")
    parser.add_argument("--started-at", default="")
    parser.add_argument("--reason", default="")
    args = parser.parse_args(argv)
    env = os.environ if environ is None else environ
    if not args.from_workflow and (args.event is None or args.outcome is None):
        parser.error("--event and --outcome are required without --from-workflow")
    url = env.get("PAPERCLIP_DEPLOY_WEBHOOK_URL", "").strip()
    secret = env.get("PAPERCLIP_DEPLOY_WEBHOOK_SECRET", "")
    if not url or not secret:
        print("::notice::deploy notify skipped: PAPERCLIP_DEPLOY_WEBHOOK_URL or "
              "PAPERCLIP_DEPLOY_WEBHOOK_SECRET is unset")
        return 0
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "https" or not parsed.netloc or parsed.username or parsed.password:
        print("::warning::deploy notify refused: the webhook URL must be a credential-free https URL")
        return 1
    if args.from_workflow:
        (args.event, args.outcome, args.version, args.image, args.previous_version,
         args.reason) = from_workflow(env)
        args.started_at = args.started_at or started_at(env)
    finished_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now()))
    record = payload(args, finished_at)
    status = send(url, secret, record, now=now, opener=opener)
    print("deploy notify: " + json.dumps({k: record[k] for k in ("event", "outcome", "sha", "versionId")},
                                         sort_keys=True) + f" -> HTTP {status}")
    if not 200 <= status < 300:
        print(f"::warning::deploy notify was not accepted (HTTP {status}); the daily release audit "
              "still cross-checks this run")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
