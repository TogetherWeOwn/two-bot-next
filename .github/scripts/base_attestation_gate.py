#!/usr/bin/env python3
"""Read-only PR metadata validation from a pull_request_target base checkout.

This produces a workflow result, NOT a custom PR-head status. Enforcement needs
native required-workflow binding; a same-name required status is insufficient.
The PR head is never checked out, imported, or executed.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import sys
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import HTTPRedirectHandler, Request, build_opener


MAX_FILES = 3000  # REST pull-request files endpoint's documented maximum.
MAX_RESPONSE_BYTES = 16 * 1024 * 1024
SELF_GATE_PATHS = frozenset({
    ".github/workflows/base-attestation.yml",
    ".github/scripts/base_attestation_gate.py",
    "scripts/ci/test_base_attestation_gate.py",
})
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
SHA = re.compile(r"[0-9a-f]{40}")
STATUSES = {"added", "removed", "modified", "renamed", "copied", "changed"}


class GateError(Exception):
    """A bounded refusal message safe to print without untrusted metadata."""


def require(condition, message):
    if not condition:
        raise GateError(message)


def positive_int(value):
    return type(value) is int and value > 0


def text(value):
    return isinstance(value, str) and bool(value)


def repository_identity(value):
    require(isinstance(value, dict), "Missing repository identity")
    require(positive_int(value.get("id"))
            and isinstance(value.get("full_name"), str)
            and REPOSITORY.fullmatch(value["full_name"]),
            "Invalid repository identity")
    return value["id"], value["full_name"]


def snapshot(pr):
    require(isinstance(pr, dict), "Invalid pull request metadata")
    require(pr.get("state") == "open" and positive_int(pr.get("number")),
            "Expected an open pull request")
    require(type(pr.get("changed_files")) is int
            and 0 < pr["changed_files"] <= MAX_FILES,
            "File inventory must contain between 1 and 3000 entries; split larger PRs")
    require(text(pr.get("title")) and isinstance(pr.get("body"), (str, type(None)))
            and text(pr.get("updated_at")), "Invalid pull request text metadata")
    user = pr.get("user")
    require(isinstance(user, dict) and text(user.get("login")), "Missing PR author")
    identity = [pr["number"], pr["state"], pr["title"], pr.get("body"),
                user["login"], pr["updated_at"], pr["changed_files"]]
    for side in ("base", "head"):
        value = pr.get(side)
        require(isinstance(value, dict) and text(value.get("ref"))
                and isinstance(value.get("sha"), str) and SHA.fullmatch(value["sha"]),
                "Invalid head or base identity")
        identity.extend((value["ref"], value["sha"], repository_identity(value.get("repo"))))
    return tuple(identity)


def inventory_path(value):
    require(isinstance(value, str) and value and value == value.strip()
            and value.splitlines() == [value]
            and not any(ord(c) < 32 or ord(c) == 127 for c in value)
            and "\\" not in value
            and all(part not in ("", ".", "..") for part in value.split("/")),
            "Unrepresentable repository-relative path")
    return value


def changed_paths(api, prefix, expected):
    paths, filenames = set(), set()
    seen = 0
    for page in range(1, (expected + 99) // 100 + 1):
        batch = api(f"{prefix}/files?per_page=100&page={page}")
        require(isinstance(batch, list) and len(batch) == min(100, expected - seen),
                "Incomplete or inconsistent file inventory")
        for row in batch:
            require(isinstance(row, dict) and isinstance(row.get("status"), str)
                    and row["status"] in STATUSES,
                    "Malformed file inventory entry")
            filename = inventory_path(row.get("filename"))
            require(filename not in filenames, "Duplicate file inventory entry")
            filenames.add(filename)
            paths.add(filename)
            old = row.get("previous_filename")
            if row["status"] == "renamed":
                require(old is not None, "Rename inventory is missing its source path")
                old = inventory_path(old)
                require(old != filename, "Rename inventory repeats its destination")
                paths.add(old)
            elif old is not None:
                # Preserve any supplied old name, even for a copied entry.
                paths.add(inventory_path(old))
        seen += len(batch)
    require(seen == expected, "Incomplete file inventory")
    return sorted(paths)


def validate(event, repository, run_sha, api, policy):
    """Validate one event snapshot using only fixed read-only REST routes.

    api(route) is injected for synthetic regressions; production uses RestReader.
    policy is the sibling base-branch pr_standards module, never a head import.
    """
    require(isinstance(event, dict) and isinstance(repository, str)
            and REPOSITORY.fullmatch(repository), "Invalid event repository")
    repo_id = repository_identity(event.get("repository"))
    require(repo_id[1] == repository, "Event repository does not match the run")
    pr = event.get("pull_request")
    expected = snapshot(pr)
    require(event.get("number") == pr["number"] and positive_int(event.get("number")),
            "Event pull request number mismatch")
    require(pr["base"]["ref"] == "main"
            and repository_identity(pr["base"].get("repo")) == repo_id,
            "Expected this repository's main base")
    require(run_sha == pr["base"]["sha"], "Run source is not the event base SHA")
    prefix = f"repos/{repository}/pulls/{pr['number']}"
    base_route = f"repos/{repository}/git/ref/heads/{quote(pr['base']['ref'], safe='')}"

    def fence():
        require(snapshot(api(prefix)) == expected, "Stale PR event; push or reopen to requalify")
        ref = api(base_route)
        require(isinstance(ref, dict) and isinstance(ref.get("object"), dict)
                and ref["object"].get("sha") == run_sha,
                "Stale base snapshot; push or reopen to requalify")

    fence()
    paths = changed_paths(api, prefix, pr["changed_files"])
    body = pr.get("body") or ""
    require(not body.strip().startswith("This release is too large to preview in the pull request body."),
            "Stored release notes require a separately qualified complete metadata route")
    findings = list(policy.evaluate({
        "EVENT": "pull_request", "TITLE": pr["title"], "BODY": body,
        "AUTHOR": pr["user"]["login"], "HEAD_REF": pr["head"]["ref"],
        "CHANGED_FILES": "\n".join(paths), "REPO_PRIVATE": "false",
        "REQUIRE_CARD_REF": "false", "PR_STANDARDS_MODE": "warn",
    }))
    if SELF_GATE_PATHS.intersection(paths):
        # Extend, never replace, the existing risk floor. The already-shipped
        # canonical gate path requests exactly the established T0 policy.
        findings.extend(policy.attestation_findings(
            body, [".github/workflows/supply-chain.yml"]))
    fence()
    require(not any(finding.level == "error" for finding in findings),
            "Base PR policy refused metadata; check title, body and T0 attestation")
    return {"head_sha": pr["head"]["sha"], "base_sha": run_sha,
            "file_count": pr["changed_files"], "path_count": len(paths)}


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise GateError("API redirects are not permitted")


class RestReader:
    def __init__(self, repository, token):
        self.prefix = f"repos/{repository}/"
        self.token = token
        self.opener = build_opener(NoRedirect())

    def __call__(self, route):
        require(route.startswith(self.prefix) and ".." not in route
                and "#" not in route, "Invalid API route")
        request = Request(f"https://api.github.com/{route}", headers={
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        }, method="GET")
        try:
            with self.opener.open(request, timeout=30) as response:
                raw = response.read(MAX_RESPONSE_BYTES + 1)
            require(len(raw) <= MAX_RESPONSE_BYTES, "API response exceeds the metadata bound")
            return json.loads(raw)
        except (HTTPError, URLError, TimeoutError, ValueError, OSError):
            # Do not expose API response bodies, URLs, PR text or token values.
            raise GateError("Read-only metadata request failed") from None


def load_policy():
    spec = importlib.util.spec_from_file_location(
        "base_pr_standards", Path(__file__).resolve().with_name("pr_standards.py"))
    policy = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = policy
    spec.loader.exec_module(policy)
    return policy


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", required=True, type=Path)
    args = parser.parse_args()
    try:
        require(os.environ.get("GITHUB_EVENT_NAME") == "pull_request_target",
                "Only a base-controlled pull_request_target run is supported")
        require(text(os.environ.get("GH_TOKEN")), "Missing read-only metadata token")
        require(args.event.stat().st_size <= MAX_RESPONSE_BYTES, "Event exceeds the metadata bound")
        event = json.loads(args.event.read_text(encoding="utf-8"))
        repository = os.environ.get("GITHUB_REPOSITORY", "")
        receipt = validate(event, repository, os.environ.get("GITHUB_SHA", ""),
                           RestReader(repository, os.environ["GH_TOKEN"]), load_policy())
        print(json.dumps(receipt, sort_keys=True))
        return 0
    except (GateError, ValueError, KeyError, TypeError, OSError):
        # Keep diagnostics constant: raw PR fields can inject workflow commands.
        print("Base attestation validation refused; inspect event freshness, inventory and PR policy.",
              file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
