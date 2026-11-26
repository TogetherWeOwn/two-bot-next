#!/usr/bin/env python3
"""Check the reviewer-notes boundary in a PR body before publication."""

import argparse
from pathlib import Path
import re
import sys

from pr_standards import T0_CHECK_ITEM, strip_comments

CHECKLIST_IDS = {f"D{index}" for index in range(1, 9)}


def check_boundary(body):
    lines = strip_comments(body).splitlines()
    headings = [(index, line) for index, line in enumerate(lines)
                if re.match(r"^#{1,2} ", line)]
    for (start, heading), (end, following) in zip(headings, headings[1:]):
        if heading != "## Attestation" or following != "## Notes for reviewers":
            continue
        section = [line.strip() for line in lines[start + 1:end] if line.strip()]
        if "### High-risk checklist" not in section:
            return False
        checklist = section[section.index("### High-risk checklist") + 1:]
        items = [T0_CHECK_ITEM.fullmatch(line) for line in checklist]
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
