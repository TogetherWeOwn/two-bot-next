#!/usr/bin/env python3
"""Validate one voice synthetic attempt without publishing its raw evidence."""

import json
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from script_intake import decode_legacy_json, unique_object as _unique_json_object
sys.path.pop(0)


def unique_object(pairs):
    return _unique_json_object(pairs, message='duplicate object key')


def validate(root, run_id):
    expected = f"voice-synthetic-{run_id}-1.json"
    try:
        names = sorted(os.listdir(root))
    except (FileNotFoundError, NotADirectoryError):
        names = []
    if not names:
        return "evidence_missing", []
    if names != [expected]:
        return "retry_inflated_or_unexpected_files", []
    path = Path(root) / expected
    try:
        if path.is_symlink() or not path.is_file() or path.stat().st_size > 262144:
            return "evidence_invalid", [expected]
        with path.open(encoding="utf-8") as source:
            checks = decode_legacy_json(source.read(), object_pairs_hook=unique_object)["checks"]
        if not isinstance(checks, list):
            raise ValueError("invalid checks")
        verdicts = {}
        for item in checks:
            name, verdict = item["check"], item["verdict"]
            if not isinstance(name, str) or name in verdicts or verdict not in ("PASS", "FAIL", "SKIP"):
                raise ValueError("invalid verdict")
            verdicts[name] = verdict
    except (OSError, ValueError, TypeError, KeyError, UnicodeError):
        return "evidence_invalid", [expected]
    if "FAIL" in verdicts.values():
        return "lifecycle_failed", [expected]
    if any(verdicts.get(name) != "PASS" for name in ("create", "moved", "delete")):
        return "lifecycle_incomplete", [expected]
    return "single_successful_lifecycle", [expected]


def main():
    if len(sys.argv) != 4 or not sys.argv[2].isascii() or not sys.argv[2].isdigit():
        print("usage: voice_lifecycle_evidence.py ROOT RUN_ID OUTPUT", file=sys.stderr)
        return 2
    code, files = validate(sys.argv[1], sys.argv[2])
    valid = code == "single_successful_lifecycle"
    output = Path(sys.argv[3])
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps({"schema": "voice-rest-lifecycle-v1", "valid": valid,
                                  "code": code, "files": files}), encoding="utf-8")
    if not valid:
        print(f"::error::voice lifecycle evidence not usable ({code})")
    return 0 if valid else 1


if __name__ == "__main__":
    sys.exit(main())
