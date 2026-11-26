#!/usr/bin/env python3
"""Check automatic GitHub Actions triggers against the CI demand policy."""

import hashlib
from pathlib import Path
import re
import sys

import yaml


FILTERABLE = {"pull_request", "pull_request_target", "push"}
PR_EVENTS = {"pull_request", "pull_request_target", "pull_request_review", "pull_request_review_comment"}
INDIRECT_OR_EXPLICIT = {"workflow_call", "workflow_dispatch", "schedule"}
CODE_EVENTS = ["opened", "synchronize", "reopened"]
METADATA_EVENTS = ["opened", "edited", "synchronize", "reopened"]
METADATA_REFRESH = {"supply-chain.yml", "pr-evidence.yml"}
MAX_WORKFLOW_BYTES = 1024 * 1024
HEADER = re.compile(r"^# guards: (\S+(?: / \S+)?)(?:[ \t]+\([^\r\n]*\))?[ \t]*\r?$", re.MULTILINE)
MAIN_AND_QUEUE = {"push": {"branches": ["main"]}, "merge_group": {"types": ["checks_requested"]}}
REVIEW_TYPES = [*CODE_EVENTS, "ready_for_review", "labeled", "unlabeled"]
ADMISSIONS = {
    "check.yml": ("release:exact-main-ci", {**MAIN_AND_QUEUE, "pull_request": {"types": CODE_EVENTS}}),
    "supply-chain.yml": ("release:main-commit-lint-and-secret-scan",
                         {**MAIN_AND_QUEUE, "pull_request": {"types": METADATA_EVENTS}}),
    "deploy-production.yml": ("release:production-promotion", {"workflow_run": {
        "workflows": ["deploy-staging", "check"], "types": ["completed"], "branches": ["main"],
    }}),
    "staging-migrate-reviewed.yml": ("release:reviewed-staging-migration", {"push": {
        "branches": ["main"], "paths": [".github/staging-apply/*.json"],
    }}),
    "pr-size.yml": ("required:pr-size-gate / gate", {
        "pull_request": {"types": REVIEW_TYPES}, "pull_request_review": {"types": ["submitted", "dismissed"]},
    }),
    "vote-kick-fuzz-smoke.yml": ("branch:ci/vote-kick-fuzz-smoke", {"push": {"branches": ["ci/vote-kick-fuzz-smoke"]}}),
    "mutation.yml": ("", {"pull_request": {"types": CODE_EVENTS}}),
    # Required Check does not replace the benchmark in nightly's full PR sweep.
    "nightly.yml": ("", {"pull_request": {}}),
    "pr-evidence.yml": ("", {
        "pull_request": {"types": [*METADATA_EVENTS, "ready_for_review"]},
        "push": {"branches": ["main"], "paths": [".github/scripts/*pr*evidence*", ".github/workflows/pr-evidence.yml"]},
    }),
    "review-rules.yml": ("", {"pull_request": {}, "merge_group": {}}),
    "test-guard.yml": ("", {"pull_request": {"types": REVIEW_TYPES}}),
}
# Admit only the historical snapshot while it awaits retirement in #2694.
LEGACY_SNAPSHOT = "5586b6fee3e8165e191d5d3c95469f50a88deb0a13491bc1caeb576bf28fff28"


class UniqueKeyLoader(yaml.BaseLoader):
    def construct_mapping(self, node, deep=False):
        mapping = {}
        for key_node, value_node in node.value:
            key = self.construct_object(key_node, deep=deep)
            if not isinstance(key, str) or key == "<<" or key in mapping:
                raise ValueError("invalid or duplicate YAML key")
            mapping[key] = self.construct_object(value_node, deep=deep)
        return mapping


def trigger_options(workflow):
    events = workflow["on"]
    if isinstance(events, str):
        events = {events: {}}
    elif isinstance(events, list) and events and all(isinstance(event, str) for event in events):
        if len(set(events)) != len(events):
            raise ValueError("duplicate workflow event")
        events = dict.fromkeys(events, {})
    if not isinstance(events, dict) or not events:
        raise ValueError("missing or invalid workflow triggers")
    settings = {}
    for event in events.keys() - INDIRECT_OR_EXPLICIT:
        options = events[event]
        if options in ("", "null", "~"):
            options = {}
        if not isinstance(options, dict):
            raise ValueError("invalid automatic trigger options")
        settings[event] = options
    return settings


def options_list(options, key):
    if key not in options:
        return None
    values = options[key]
    if not isinstance(values, list) or not values or not all(
            isinstance(value, str) and value.strip() for value in values):
        raise ValueError(f"invalid {key} filter")
    return values


def can_push_main(options):
    branches = options_list(options, "branches")
    return branches is None or "main" in branches or any(
        re.search(r"[^A-Za-z0-9._/-]", branch) for branch in branches)


def has_path_filter(event, options):
    if event == "push" and (not options_list(options, "branches") or {"tags", "tags-ignore"} & options.keys()):
        return False
    positives = [path for path in options_list(options, "paths") or () if not path.startswith("!")]
    return "paths-ignore" not in options and bool(positives) and all(
        re.match(r"^[A-Za-z0-9_.-]+(?:/|$)", path) for path in positives)


def findings(name, data):
    text = data.decode("utf-8")
    workflow = yaml.load(text, Loader=UniqueKeyLoader)
    events = trigger_options(workflow)
    if not events and name not in ADMISSIONS:
        return []
    markers = HEADER.findall(text)
    guard = markers[0] if len(markers) == 1 else ""
    options = events.get("pull_request")
    # #2648 owns removal of this transitional draft-ready refresh.
    if name == "supply-chain.yml" and options == {"types": [*METADATA_EVENTS, "ready_for_review"]}:
        events["pull_request"] = {"types": METADATA_EVENTS}
    admitted = ADMISSIONS.get(name) == (guard, events)
    if name == "pr-size.yml":
        job = workflow["jobs"].get("pr-size-gate", {})
        admitted = admitted and job.get("name") == "pr-size-gate"
    release = admitted and guard.startswith("release:")
    legacy = name == "configuration-reference.yml" and hashlib.sha256(data).hexdigest() == LEGACY_SNAPSHOT

    errors = []
    if name in ADMISSIONS and not admitted:
        errors.append("automatic triggers differ from the reviewed workflow contract")
    elif guard == "paths":
        for event, options in sorted(events.items()):
            if event not in FILTERABLE:
                errors.append("an automatic event does not support a paths filter")
            elif not has_path_filter(event, options):
                errors.append(f"{event} declares no paths filter")
    elif not (admitted or legacy):
        errors.append("automatic triggers need a required, release, paths or dedicated CI branch guard")
    if len(markers) > 1:
        errors.append("multiple guard declarations")
    pr_events = events.keys() & PR_EVENTS
    for event in sorted(pr_events):
        activities = options_list(events[event], "types") or []
        refresh = event == "pull_request" and name in METADATA_REFRESH and admitted
        if "edited" in activities and not refresh:
            errors.append(f"{event} types include 'edited'")
    if "workflow_run" in events:
        if options_list(events["workflow_run"], "types") != ["completed"]:
            errors.append("workflow_run must fire only on completed, never requested/in_progress")
        if not release:
            errors.append("workflow_run needs a reviewed release guard")
    evidence_change = name == "pr-evidence.yml" and admitted
    if pr_events and "push" in events and can_push_main(events["push"]) and not (release or evidence_change):
        errors.append("runs on PRs and main pushes without a reviewed release guard")
    return errors


def check_directory(directory):
    if directory.is_symlink() or directory.parent.is_symlink() or not directory.is_dir():
        return ["Missing or invalid workflows directory"]
    paths = sorted(path for path in directory.iterdir() if path.suffix.lower() in (".yml", ".yaml"))
    if not paths:
        return ["No workflows found"]
    errors = []
    for path in paths:
        try:
            if path.is_symlink() or not path.is_file():
                raise ValueError("invalid workflow file")
            with path.open("rb") as file:
                data = file.read(MAX_WORKFLOW_BYTES + 1)
            if len(data) > MAX_WORKFLOW_BYTES:
                raise ValueError("workflow too large")
            problems = findings(path.name, data)
        except (AttributeError, KeyError, OSError, RecursionError, TypeError, ValueError, yaml.YAMLError):
            problems = ["unreadable YAML or invalid workflow trigger configuration"]
        if problems:
            name = path.name if re.fullmatch(r"[\w.-]+", path.name, re.ASCII) else "Invalid workflow name"
            errors.extend(f"{name}: {problem}" for problem in problems)
    return errors


def main(paths):
    errors = []
    for path in map(Path, paths or ["."]):
        directory = path / ".github/workflows" if (path / ".github").exists() else path
        errors.extend(check_directory(directory))
    if errors:
        print("\n".join(errors))
        return 1
    print("Workflow demand guard passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
