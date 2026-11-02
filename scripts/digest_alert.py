#!/usr/bin/env python3
"""File one `incident` GitHub issue for a missing daily digest (stdlib only).

Fires when production changed but no digest tag was made — the silent-failure
mode that lost tags when the release job skipped with no alert. Uses the
shared incident sink (`incident_issue.open_incident`: one open issue for the
`daily-digest` key, later misses comment on it, fixed-vocabulary failures).
The agent who cuts the missing tag closes the issue.

Usage (called only from the daily-digest workflow alert step):
  GITHUB_TOKEN=<token> RUN_ID=<run> \\
      python3 scripts/digest_alert.py --live-sha <sha> --prev-tag <tag>
"""

import argparse
import importlib
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
# Dynamic sibling import (same pattern as test_synthetic_alert.py): a plain
# `from x import y` after sys.path manipulation would need a lint waiver,
# which the floor guard forbids on new lines.
_synthetic = importlib.import_module("synthetic_alert")
incident_issue = importlib.import_module("incident_issue")
MAX_CONTENT = _synthetic.MAX_CONTENT
RUNBOOK_URL = _synthetic.RUNBOOK_URL
run_link = incident_issue.run_link
INCIDENT_KEY = "daily-digest"

SHA = re.compile(r"[0-9a-f]{40}")
# Legacy daily digests (vYYYY.MM.DD) or stable CalVer digests (vYYYY.MDD.P).
TAG = re.compile(r"v[0-9]{4}\.(?:[0-9]{2}\.[0-9]{2}|[1-9][0-9]{2,3}\.(?:0|[1-9][0-9]*))")


def printable(value):
    return value if isinstance(value, str) and value.isascii() and value.isprintable() else None


def build_content(live_sha, prev_tag, link):
    sha7 = live_sha[:7] if isinstance(live_sha, str) and SHA.fullmatch(live_sha) else "unknown"
    prev = printable(prev_tag) if prev_tag else "none"
    if prev is None:
        prev = "unknown"
    content = (f"two-bot-next ALERT daily-digest: production changed ({sha7}) "
               f"but no digest tag was made (prev {prev}). "
               f"Runbook: {RUNBOOK_URL} | logs {link}")
    return content if len(content) <= MAX_CONTENT else content[:MAX_CONTENT - 3] + "..."


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--live-sha", required=True, help="live production build revision")
    parser.add_argument("--prev-tag", default="", help="previous digest tag (empty when none)")
    args = parser.parse_args(argv)
    if not SHA.fullmatch(args.live_sha or ""):
        print("digest_alert: refusing: live SHA is not 40 hex", file=sys.stderr)
        return 1
    if args.prev_tag and not TAG.fullmatch(args.prev_tag):
        print("digest_alert: refusing: prev tag is not vYYYY.MM.DD or vYYYY.MDD.P", file=sys.stderr)
        return 1
    content = build_content(args.live_sha, args.prev_tag, run_link())
    print(content)
    body = (f"{content}\n\nNext: cut or repair the missing digest per docs/releases.md, "
            "then close this issue.")
    try:
        action, number = incident_issue.open_incident(
            os.environ, INCIDENT_KEY, "daily-digest: production changed without a digest tag", body)
    except incident_issue.IncidentError as error:
        print(f"digest_alert: {error}", file=sys.stderr)
        return 1
    print(f"digest_alert: delivered missing-digest alert: {action} issue #{number}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
