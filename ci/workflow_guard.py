#!/usr/bin/env python3
"""Check automatic GitHub Actions triggers against the CI demand policy."""

import hashlib
from pathlib import Path
import re
import sys

import yaml


AUTOMATIC = {"pull_request", "pull_request_target", "push", "workflow_run"}
PR_EVENTS = {"pull_request", "pull_request_target"}
CODE_EVENTS = {"opened", "synchronize", "reopened"}
METADATA_EVENTS = CODE_EVENTS | {"ready_for_review"}
MAX_WORKFLOW_BYTES = 1024 * 1024
HEADER = re.compile(r"^# guards: (\S+)(?:[ \t]+\([^\r\n]*\))?[ \t]*$", re.MULTILINE)
RELEASES = {
    "check.yml": "release:exact-main-ci",
    "supply-chain.yml": "release:main-commit-lint-and-secret-scan",
    "deploy-production.yml": "release:production-promotion",
    "staging-migrate-reviewed.yml": "release:reviewed-staging-migration",
}
# These existing advisory checks need to see PR changes across the repository.
# Bound their automatic surfaces so an annotation cannot add new triggers.
ADVISORY_PR_TYPES = {
    "mutation.yml": CODE_EVENTS,
    "pr-evidence.yml": METADATA_EVENTS,
    "review-rules.yml": CODE_EVENTS,
    "test-guard.yml": METADATA_EVENTS | {"labeled", "unlabeled"},
}
# The one-off writer is pending retirement in #2694. Only its fixed historical
# source and PR condition are admitted, and deleting the workflow also passes.
LEGACY_SNAPSHOT = "5586b6fee3e8165e191d5d3c95469f50a88deb0a13491bc1caeb576bf28fff28"


class UniqueKeyLoader(yaml.BaseLoader):
    def construct_mapping(self, node, deep=False):
        mapping = {}
        for key_node, value_node in node.value:
            key = self.construct_object(key_node, deep=deep)
            if not isinstance(key, str) or key in mapping:
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
    for event in events.keys() & AUTOMATIC:
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
    if branches is None:
        return options_list(options, "tags") is None and options_list(options, "tags-ignore") is None
    # Literal branches that exclude main are the only branch lists the guard
    # treats as off-main. GitHub's negation and glob order can admit main.
    return "main" in branches or any(re.search(r"[^A-Za-z0-9._/-]", branch) for branch in branches)


def has_path_filter(options):
    paths = options_list(options, "paths")
    ignored = options_list(options, "paths-ignore")
    return bool(paths) != bool(ignored)


def findings(name, data):
    text = data.decode("utf-8")
    workflow = yaml.load(text, Loader=UniqueKeyLoader)
    events = trigger_options(workflow)
    if not events:
        return []
    job_names = {job.get("name", key) for key, job in workflow["jobs"].items()}
    markers = HEADER.findall(text)
    guard = markers[0] if len(markers) == 1 else ""
    release = RELEASES.get(name) == guard
    pr_types = {event: set(options_list(events[event], "types") or CODE_EVENTS)
                for event in events.keys() & PR_EVENTS}
    approved_advisory = (set(events) == {"pull_request"} and name in ADVISORY_PR_TYPES
                         and pr_types["pull_request"] <= ADVISORY_PR_TYPES[name])
    legacy = name == "configuration-reference.yml" and hashlib.sha256(data).hexdigest() == LEGACY_SNAPSHOT
    branch = guard.removeprefix("branch:")
    approved_branch = (guard.startswith("branch:ci/") and set(events) == {"push"}
                       and re.fullmatch(r"ci/[A-Za-z0-9._/-]+", branch)
                       and events["push"] == {"branches": [branch]})
    required = guard.startswith("required:") and guard.removeprefix("required:") in job_names

    errors = []
    if guard == "paths":
        for event, options in sorted(events.items()):
            if event == "workflow_run" or not has_path_filter(options):
                errors.append(f"{event} declares no paths filter")
    elif not any((release, required, approved_advisory, approved_branch, legacy)):
        errors.append("automatic triggers need a required, release, paths or dedicated CI branch guard")
    if len(markers) > 1:
        errors.append("multiple guard declarations")
    for event, activities in sorted(pr_types.items()):
        if "edited" in activities and (name, guard, event) != (
                "supply-chain.yml", RELEASES["supply-chain.yml"], "pull_request"):
            errors.append(f"{event} types include 'edited'")
    if "workflow_run" in events:
        if options_list(events["workflow_run"], "types") != ["completed"]:
            errors.append("workflow_run must fire only on completed, never requested/in_progress")
        if not release:
            errors.append("workflow_run needs a reviewed release guard")
    if pr_types and "push" in events and can_push_main(events["push"]) and not release:
        errors.append("runs on PRs and main pushes without a reviewed release guard")
    return errors


def check_directory(directory):
    if directory.is_symlink() or directory.parent.is_symlink() or not directory.is_dir():
        return ["Missing or invalid workflows directory"]
    paths = sorted(path for path in directory.iterdir() if path.suffix in (".yml", ".yaml"))
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
