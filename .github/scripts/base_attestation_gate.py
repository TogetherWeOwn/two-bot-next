#!/usr/bin/env python3
"""Inactive attestation CLI adapter, adapted from Rick7C2's original validator.

This is not wired to a workflow or an enforcement gate. The caller must establish
trusted provenance, provider consistency, snapshot semantics and live fencing.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import sys

from attestation_inventory import evaluate_bracketed
from attestation_transport import RestReader

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
from script_intake import decode_legacy_json, read_with_overflow, unique_object
sys.path.pop(0)


MAX_EVENT_BYTES = 8 * 1024 * 1024
REFUSAL = "Attestation validation refused."


def _constant(_value):
    raise ValueError


def load_policy():
    spec = importlib.util.spec_from_file_location(
        "base_pr_standards", Path(__file__).resolve().with_name("pr_standards.py"))
    policy = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = policy
    spec.loader.exec_module(policy)
    return policy


class _Parser(argparse.ArgumentParser):
    def error(self, _message):
        raise ValueError


def main(argv=None):
    try:
        parser = _Parser(add_help=False, exit_on_error=False)
        parser.add_argument("--event", required=True, type=Path)
        args = parser.parse_args(argv)
        if os.environ.get("GITHUB_EVENT_NAME") != "pull_request_target":
            raise ValueError
        with args.event.open("rb") as source:
            raw = read_with_overflow(source, MAX_EVENT_BYTES)
        if len(raw) > MAX_EVENT_BYTES:
            raise ValueError
        event = decode_legacy_json(raw.decode("utf-8"), object_pairs_hook=unique_object,
                                   parse_constant=_constant)
        repository = os.environ.get("GITHUB_REPOSITORY", "")
        reader = RestReader(repository, os.environ.get("GH_TOKEN", ""))
        result = evaluate_bracketed(event, repository, os.environ.get("GITHUB_SHA", ""),
                                    reader, load_policy())
        print(json.dumps(result, sort_keys=True))
        return 0
    except (Exception, SystemExit):
        print(REFUSAL, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
