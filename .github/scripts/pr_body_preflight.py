#!/usr/bin/env python3
"""Check the reviewer-notes boundary in a PR body before publication."""

import argparse
from pathlib import Path
import re
import sys

from pr_standards import T0_CHECK_ITEM, strip_comments

CHECKLIST_IDS = {f"D{index}" for index in range(1, 9)}
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")


def visible_lines(lines):
    """Keep line offsets while skipping fenced code examples."""
    fence = ""
    for index, line in enumerate(lines):
        marker = FENCE.match(line)
        if fence:
            if marker and marker[1].startswith(fence) and not marker[2].strip():
                fence = ""
        elif marker and (marker[1].startswith("~") or "`" not in marker[2]):
            fence = marker[1]
        else:
            yield index, line


def check_boundary(body):
    lines = strip_comments(body).splitlines()
    visible = list(visible_lines(lines))
    headings = [(index, line) for index, line in visible if line.startswith(("# ", "## "))]
    for (start, heading), (end, following) in zip(headings, headings[1:]):
        if heading != "## Attestation" or following != "## Notes for reviewers":
            continue
        checklist_starts = [index for index, line in visible
                            if start < index < end and line.rstrip() == "### High-risk checklist"]
        if len(checklist_starts) != 1:
            return False
        # Use the raw lines here so a fence after the checklist starts still fails.
        checklist = lines[checklist_starts[0] + 1:end]
        items = [T0_CHECK_ITEM.fullmatch(line.strip()) for line in checklist if line.strip()]
        return len(items) == len(CHECKLIST_IDS) and CHECKLIST_IDS == {
            item[2] for item in items if item is not None
        }
    return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("body", type=Path, help="final UTF-8 PR body file")
    args = parser.parse_args()
    try:
        body = args.body.read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        print("Could not read the PR body as UTF-8.", file=sys.stderr)
        return 1
    if not check_boundary(body):
        print("Keep `## Notes for reviewers` immediately after the Attestation checklist. "
              "Place attribution and generated footers under reviewer notes.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
