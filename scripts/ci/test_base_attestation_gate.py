"""Offline tests for the base attestation gate.

The gate (`.github/scripts/base_attestation_gate.py`) runs from a
`pull_request_target` workflow, checks out only the base branch's
`.github/scripts`, reads the pull request through fixed read-only REST routes
and judges the T0 attestation with the already-merged `pr_standards.py`. These
tests run it against a synthetic in-memory API: no network, no token, no head
checkout. Source tests pin the workflow shape (trigger, permissions, pinned
checkout of the base SHA, no head execution).

What a source test cannot prove: a check run is only a required check when a
ruleset or required workflow binds it. A same-named check from another
workflow could spoof it, so the tests assert the check carries its own distinct
name and that the docs state the required-workflow binding plainly instead of
claiming runtime enforcement.
"""

import copy
import importlib.util
import inspect
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from collections import namedtuple
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
GATE_PATH = ROOT / ".github" / "scripts" / "base_attestation_gate.py"
POLICY_PATH = ROOT / ".github" / "scripts" / "pr_standards.py"
WORKFLOW_PATH = ROOT / ".github" / "workflows" / "base-attestation.yml"
WORKFLOWS_DIR = ROOT / ".github" / "workflows"


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


gate = load("_bag_gate_under_test", GATE_PATH)
policy = load("_bag_real_policy", POLICY_PATH)

REPO = "TogetherWeOwn/two-bot-next"
REPO_ID = 4242
NUMBER = 7
BASE_SHA = "b" * 40
HEAD_SHA = "a" * 40
OTHER_SHA = "c" * 40

PR_ROUTE = f"repos/{REPO}/pulls/{NUMBER}"
REF_ROUTE = f"repos/{REPO}/git/ref/heads/main"
FILES_RE = re.compile(
    rf"repos/{re.escape(REPO)}/pulls/{NUMBER}/files\?per_page=100&page=(\d+)"
)

PUBLIC_BODY = """
## Linked Issues or Issue Description

Closes #123

## Verification

- `python3 scripts/ci/test_base_attestation_gate.py` passes
"""

FULL_ATTESTATION = """
## Attestation

risk_tier: T0

### Claims

- C1: the gate rejects a stale head snapshot

### Tests

- C1 | test=scripts/ci/test_base_attestation_gate.py::test_stale | failed_before=not-run | proof=python3 scripts/ci/test_base_attestation_gate.py OK

### High-risk checklist

- [x] D1: no secret sinks; redacted evidence only
- [x] D2: no auth paths changed
- [x] D3: no migrations shipped
- [x] D4: bounded inputs and caps
- [x] D5: no concurrency hazard
- [x] D6: pinned actions only
- [x] D7: failures propagate honestly
- [x] D8: malformed shapes covered
"""

Finding = namedtuple("Finding", "level title message")


def pull_request(**over):
    pr = {
        "number": NUMBER,
        "state": "open",
        "title": "fix(gate): bind base attestation",
        "body": PUBLIC_BODY,
        "user": {"login": "contributor"},
        "updated_at": "2026-11-08T10:00:00Z",
        "changed_files": 1,
        "head": {
            "ref": "fix/slug",
            "sha": HEAD_SHA,
            "repo": {"id": REPO_ID, "full_name": REPO},
        },
        "base": {
            "ref": "main",
            "sha": BASE_SHA,
            "repo": {"id": REPO_ID, "full_name": REPO},
        },
    }
    for dotted, value in over.items():
        set_path(pr, dotted.replace("__", "."), value)
    return pr


def set_path(obj, dotted, value):
    *parents, leaf = dotted.split(".")
    for key in parents:
        obj = obj[key]
    obj[leaf] = value


def del_path(obj, dotted):
    *parents, leaf = dotted.split(".")
    for key in parents:
        obj = obj[key]
    del obj[leaf]


def event_for(pr):
    return {
        "action": "opened",
        "number": pr["number"],
        "repository": {"id": REPO_ID, "full_name": REPO},
        "pull_request": copy.deepcopy(pr),
    }


def changed(name, status="modified", **extra):
    return {"filename": name, "status": status, **extra}


class FakeApi:
    """Read-only synthetic REST API. Any route outside the fixed three fails."""

    def __init__(self, pr, files, ref_sha=BASE_SHA):
        self.pr = copy.deepcopy(pr)
        self.files = copy.deepcopy(files)
        self.ref_sha = ref_sha
        self.calls = []
        self.after_files = None

    def __call__(self, route):
        self.calls.append(route)
        if route == PR_ROUTE:
            return copy.deepcopy(self.pr)
        if route == REF_ROUTE:
            return {
                "ref": "refs/heads/main",
                "object": {"sha": self.ref_sha, "type": "commit"},
            }
        match = FILES_RE.fullmatch(route)
        if match:
            page = int(match.group(1))
            out = copy.deepcopy(self.files[(page - 1) * 100 : page * 100])
            if self.after_files:
                self.after_files(self)
            return out
        raise AssertionError(f"unexpected route {route!r}")

    def file_pages(self):
        return [int(m.group(1)) for r in self.calls if (m := FILES_RE.fullmatch(r))]


class RecordingPolicy:
    def __init__(self, findings=(), attestation=(), on_evaluate=None):
        self.findings = list(findings)
        self.attestation = list(attestation)
        self.on_evaluate = on_evaluate
        self.envs = []
        self.attested = []

    def evaluate(self, env):
        self.envs.append(dict(env))
        if self.on_evaluate:
            self.on_evaluate()
        return list(self.findings)

    def attestation_findings(self, body, paths):
        self.attested.append((body, list(paths)))
        return list(self.attestation)


def make_files(n, prefix="src/f"):
    return [changed(f"{prefix}{i}.txt") for i in range(n)]


UNSET = object()


class Harness(unittest.TestCase):
    def setup(self, files=None, pr=None, **pr_over):
        files = [changed("src/lib.rs")] if files is None else files
        pr = pr if pr is not None else pull_request(
            **{"changed_files": len(files), **pr_over})
        self.pr = pr
        self.api = FakeApi(pr, files)
        self.event = event_for(pr)
        self.pol = RecordingPolicy()
        return self

    def run_gate(self, event=UNSET, repository=REPO, run_sha=BASE_SHA, pol=None):
        return gate.validate(
            self.event if event is UNSET else event,
            repository,
            run_sha,
            self.api,
            self.pol if pol is None else pol,
        )

    def assertRejected(self, **kw):
        with self.assertRaises(gate.GateError) as ctx:
            self.run_gate(**kw)
        return ctx.exception


class HappyPath(Harness):
    def test_receipt_for_simple_pr(self):
        self.setup()
        self.assertEqual(
            self.run_gate(),
            {"head_sha": HEAD_SHA, "base_sha": BASE_SHA, "file_count": 1, "path_count": 1},
        )

    def test_only_fixed_read_only_routes_are_used(self):
        self.setup(files=make_files(150))
        self.run_gate()
        allowed = re.compile(
            rf"repos/{re.escape(REPO)}/(?:pulls/{NUMBER}"
            rf"|git/ref/heads/main"
            rf"|pulls/{NUMBER}/files\?per_page=100&page=[12])"
        )
        self.assertTrue(self.api.calls)
        for route in self.api.calls:
            self.assertRegex(route, allowed)
            self.assertIsNotNone(allowed.fullmatch(route), route)
        self.assertIn(PR_ROUTE, self.api.calls)
        self.assertIn(REF_ROUTE, self.api.calls)

    def test_api_is_a_single_argument_callable(self):
        self.setup()
        sig = inspect.signature(self.api.__call__)
        self.assertEqual(list(sig.parameters), ["route"])
        self.run_gate()

    def test_policy_environment_is_pinned(self):
        files = [changed("src/a.rs"), changed("src/b.rs", "added")]
        self.setup(files=files)
        self.run_gate()
        self.assertEqual(len(self.pol.envs), 1)
        env = self.pol.envs[0]
        self.assertEqual(env["EVENT"], "pull_request")
        self.assertEqual(env["TITLE"], self.pr["title"])
        self.assertEqual(env["BODY"], self.pr["body"])
        self.assertEqual(env["AUTHOR"], "contributor")
        self.assertEqual(env["HEAD_REF"], "fix/slug")
        self.assertEqual(env["REPO_PRIVATE"], "false")
        self.assertEqual(env["REQUIRE_CARD_REF"], "false")
        self.assertEqual(env["PR_STANDARDS_MODE"], "warn")
        self.assertEqual(set(env["CHANGED_FILES"].splitlines()), {"src/a.rs", "src/b.rs"})

    def test_warnings_do_not_fail(self):
        self.setup(files=[changed(".github/workflows/base-attestation.yml")])
        pol = RecordingPolicy(
            findings=[Finding("warning", "PR template", "x")],
            attestation=[Finding("warning", "Attestation", "x")],
        )
        self.assertEqual(self.run_gate(pol=pol)["file_count"], 1)

    def test_all_valid_statuses_pass(self):
        files = [
            changed("a.txt", "added"),
            changed("b.txt", "removed"),
            changed("c.txt", "modified"),
            changed("d.txt", "renamed", previous_filename="old/d.txt"),
            changed("e.txt", "copied"),
            changed("f.txt", "changed"),
        ]
        self.setup(files=files)
        receipt = self.run_gate()
        self.assertEqual(receipt["file_count"], 6)
        self.assertEqual(receipt["path_count"], 7)

    def test_fork_head_is_allowed_and_never_fetched(self):
        pr = pull_request(
            head__repo__id=999001, head__repo__full_name="someone/two-bot-next"
        )
        self.setup(pr=pr)
        self.assertEqual(self.run_gate()["head_sha"], HEAD_SHA)
        for route in self.api.calls:
            self.assertNotIn("someone", route)
            self.assertNotIn(HEAD_SHA, route)

    def test_fork_identity_swap_is_rejected(self):
        pr = pull_request(
            head__repo__id=999001, head__repo__full_name="someone/two-bot-next"
        )
        self.setup(pr=pr)
        self.api.pr["head"]["repo"] = {"id": 999002, "full_name": "evil/two-bot-next"}
        self.assertRejected()
        self.setup(pr=pr)
        self.api.pr["head"]["repo"]["id"] = 999002
        self.assertRejected()
        self.setup(pr=pr)
        self.api.pr["head"]["repo"]["full_name"] = "evil/two-bot-next"
        self.assertRejected()

    def test_rename_reports_both_paths(self):
        files = [changed("docs/new.md", "renamed", previous_filename="docs/old.md")]
        self.setup(files=files)
        receipt = self.run_gate()
        self.assertEqual((receipt["file_count"], receipt["path_count"]), (1, 2))
        lines = set(self.pol.envs[0]["CHANGED_FILES"].splitlines())
        self.assertLessEqual({"docs/new.md", "docs/old.md"}, lines)


class IdentityAndFreshness(Harness):
    LIVE_MUTATIONS = {
        "title": "fix(gate): something else",
        "body": PUBLIC_BODY + "\nedited after the event\n",
        "user__login": "someone-else",
        "head__ref": "fix/other",
        "head__sha": OTHER_SHA,
        "head__repo__id": 1,
        "head__repo__full_name": "evil/two-bot-next",
        "base__ref": "release",
        "base__sha": OTHER_SHA,
        "base__repo__id": 1,
        "base__repo__full_name": "evil/two-bot-next",
        "updated_at": "2026-11-08T11:00:00Z",
        "changed_files": 2,
        "state": "closed",
        "number": NUMBER + 1,
    }

    def test_live_pr_drift_is_rejected(self):
        for dotted, value in self.LIVE_MUTATIONS.items():
            with self.subTest(live=dotted):
                self.setup()
                set_path(self.api.pr, dotted.replace("__", "."), value)
                self.assertRejected()

    def test_event_drift_is_rejected(self):
        for dotted, value in self.LIVE_MUTATIONS.items():
            with self.subTest(event=dotted):
                self.setup()
                set_path(self.event["pull_request"], dotted.replace("__", "."), value)
                self.assertRejected()

    def test_merged_or_closed_event_is_rejected(self):
        for state in ("closed", "merged", "draft", ""):
            with self.subTest(state=state):
                self.setup()
                self.event["pull_request"]["state"] = state
                self.api.pr["state"] = state
                self.assertRejected()

    def test_non_main_base_is_rejected_even_when_consistent(self):
        self.setup(base__ref="release")
        self.assertRejected()

    def test_event_number_mismatches(self):
        self.setup()
        self.event["number"] = NUMBER + 1
        self.assertRejected()
        self.setup()
        self.event["pull_request"]["number"] = NUMBER + 1
        self.assertRejected()

    def test_event_repository_mismatches(self):
        self.setup()
        self.event["repository"]["id"] = REPO_ID + 1
        self.assertRejected()
        self.setup()
        self.event["repository"]["full_name"] = "Other/two-bot-next"
        self.assertRejected()
        self.setup()
        self.assertRejected(repository="Other/two-bot-next")

    def test_base_repository_must_match_event_repository(self):
        self.setup(base__repo__id=REPO_ID + 1)
        self.assertRejected()
        self.setup(base__repo__full_name="Other/two-bot-next")
        self.assertRejected()

    def test_run_sha_must_be_base_sha(self):
        self.setup()
        self.assertRejected(run_sha=OTHER_SHA)
        self.assertRejected(run_sha=HEAD_SHA)
        self.assertRejected(run_sha="")

    def test_current_base_branch_must_still_be_base_sha(self):
        self.setup()
        self.api.ref_sha = OTHER_SHA
        self.assertRejected()

    def test_base_ref_moving_after_inventory_is_rejected(self):
        self.setup()

        def move(api):
            api.ref_sha = OTHER_SHA

        self.api.after_files = move
        self.assertRejected()

    def test_pr_edited_after_inventory_is_rejected(self):
        for dotted, value in (
            ("body", PUBLIC_BODY + "\nlate edit\n"),
            ("head__sha", OTHER_SHA),
            ("updated_at", "2026-11-08T12:00:00Z"),
            ("state", "closed"),
        ):
            with self.subTest(dotted=dotted):
                self.setup()
                self.api.after_files = lambda api, d=dotted, v=value: set_path(
                    api.pr, d.replace("__", "."), v
                )
                self.assertRejected()

    def test_pr_or_base_moving_during_policy_is_rejected(self):
        self.setup()
        pol = RecordingPolicy(
            on_evaluate=lambda: set_path(self.api.pr, "head.sha", OTHER_SHA)
        )
        self.assertRejected(pol=pol)
        self.setup()
        pol = RecordingPolicy(on_evaluate=lambda: setattr(self.api, "ref_sha", OTHER_SHA))
        self.assertRejected(pol=pol)

    def test_missing_or_malformed_event_shapes_raise_gate_error(self):
        for event in (None, {}, [], "x", {"pull_request": []}, {"number": NUMBER}):
            with self.subTest(event=event):
                self.setup()
                self.assertRejected(event=event)
        for dotted in (
            "title",
            "user.login",
            "updated_at",
            "changed_files",
            "head.sha",
            "head.ref",
            "head.repo.id",
            "base.sha",
            "base.ref",
            "base.repo.full_name",
        ):
            with self.subTest(missing=dotted):
                self.setup()
                del_path(self.event["pull_request"], dotted)
                self.assertRejected()
        for key in ("id", "full_name"):
            with self.subTest(missing_repo=key):
                self.setup()
                del self.event["repository"][key]
                self.assertRejected()

    def test_live_pr_not_a_mapping_is_rejected(self):
        self.setup()
        self.api.pr = []
        self.assertRejected()


class Inventory(Harness):
    def test_exact_count_of_100_needs_no_extra_page(self):
        self.setup(files=make_files(100))
        self.assertEqual(self.run_gate()["file_count"], 100)
        self.assertEqual(set(self.api.file_pages()), {1})

    def test_exact_count_of_200_reads_two_pages_only(self):
        self.setup(files=make_files(200))
        self.assertEqual(self.run_gate()["file_count"], 200)
        self.assertEqual(set(self.api.file_pages()), {1, 2})

    def test_count_of_150_reads_two_pages(self):
        self.setup(files=make_files(150))
        self.assertEqual(self.run_gate()["file_count"], 150)
        self.assertEqual(set(self.api.file_pages()), {1, 2})

    def test_max_count_3000_is_accepted_with_bounded_pagination(self):
        self.setup(files=make_files(3000))
        receipt = self.run_gate()
        self.assertEqual((receipt["file_count"], receipt["path_count"]), (3000, 3000))
        self.assertEqual(max(self.api.file_pages()), 30)

    def test_count_over_3000_is_rejected_without_paging_forever(self):
        self.setup(files=make_files(3001), changed_files=3001)
        self.assertRejected()
        self.assertLessEqual(len(self.api.calls), 100)

    def test_count_zero_negative_or_not_an_int_is_rejected(self):
        for value in (0, -1, True, False, "1", 1.0, None):
            with self.subTest(value=value):
                self.setup(files=[changed("src/a.rs")])
                self.event["pull_request"]["changed_files"] = value
                self.api.pr["changed_files"] = value
                self.assertRejected()

    def test_truncated_inventory_is_rejected(self):
        self.setup(files=make_files(2), changed_files=3)
        self.assertRejected()
        self.setup(files=make_files(250), changed_files=250)
        self.api.files = self.api.files[:150]
        self.assertRejected()
        self.setup(files=make_files(150), changed_files=150)
        self.api.files = self.api.files[:50] + self.api.files[100:]
        self.assertRejected()

    def test_surplus_inventory_is_rejected(self):
        self.setup(files=make_files(4), changed_files=3)
        self.assertRejected()
        self.setup(files=make_files(101), changed_files=101)
        self.api.files.append(changed("extra/one.txt"))
        self.assertRejected()
        self.assertLessEqual(max(self.api.file_pages()), 2)

    def test_oversized_page_is_rejected(self):
        self.setup(files=make_files(101), changed_files=101)
        real_call = self.api.__call__

        def fat(route):
            out = real_call(route)
            if route.endswith("page=1"):
                return out + [changed("extra.txt")]
            return out

        self.api = fat
        self.assertRejected()

    def test_non_list_pages_are_rejected(self):
        for payload in ({}, {"files": []}, None, "x"):
            with self.subTest(payload=payload):
                self.setup()
                real_call = self.api.__call__
                self.api = lambda route, rc=real_call, p=payload: (
                    p if FILES_RE.fullmatch(route) else rc(route)
                )
                self.assertRejected()

    def test_duplicate_current_filename_is_rejected(self):
        self.setup(files=[changed("src/a.rs"), changed("src/b.rs"), changed("src/a.rs")],
                   changed_files=3)
        self.assertRejected()
        files = make_files(100) + [changed("src/f5.txt")]
        self.setup(files=files, changed_files=101)
        self.assertRejected()

    def test_malformed_entries_are_rejected(self):
        bad_entries = [
            "src/a.rs",
            None,
            [],
            {"status": "modified"},
            {"filename": "src/a.rs"},
            {"filename": 7, "status": "modified"},
            {"filename": "src/a.rs", "status": 7},
            {"filename": "src/a.rs", "status": "exploded"},
            {"filename": "src/a.rs", "status": "Modified"},
            {"filename": "src/a.rs", "status": ""},
            {"filename": "src/a.rs", "status": "renamed"},
            {"filename": "src/a.rs", "status": "renamed", "previous_filename": ""},
            {"filename": "src/a.rs", "status": "renamed", "previous_filename": 3},
            {"filename": "src/a.rs", "status": "renamed", "previous_filename": "../x"},
        ]
        for entry in bad_entries:
            with self.subTest(entry=entry):
                self.setup(files=[entry], changed_files=1)
                self.assertRejected()

    def test_non_canonical_paths_are_rejected(self):
        bad_paths = [
            "",
            " src/a.rs",
            "src/a.rs ",
            "\tsrc/a.rs",
            "src/a.rs\n",
            "src/a\nrs",
            "src/a\x00.rs",
            "src/a\x1f.rs",
            "src/a\x7f.rs",
            "src/a\u0085.rs",
            "src/a" + chr(0x2028) + ".rs",
            "src/a" + chr(0x2029) + ".rs",
            "src\\a.rs",
            "/etc/passwd",
            "/src/a.rs",
            "src/../a.rs",
            "../a.rs",
            "..",
            ".",
            "./a.rs",
            "src/./a.rs",
            "src//a.rs",
            "src/",
            "src/a.rs/",
            "C:\\a.rs",
        ]
        for path in bad_paths:
            with self.subTest(path=path, role="filename"):
                self.setup(files=[changed(path)], changed_files=1)
                self.assertRejected()
            with self.subTest(path=path, role="previous_filename"):
                self.setup(
                    files=[changed("src/new.rs", "renamed", previous_filename=path)],
                    changed_files=1,
                )
                self.assertRejected()

    def test_odd_but_canonical_paths_are_accepted(self):
        for path in (".github/workflows/ci.yml", "a b/c d.txt", "dir/.hidden", "a..b/c.rs",
                     "src/..foo", "unicode/\u00e9.rs"):
            with self.subTest(path=path):
                self.setup(files=[changed(path)], changed_files=1)
                self.assertEqual(self.run_gate()["path_count"], 1)

    def test_inventory_never_asks_for_contents_or_the_head(self):
        self.setup(files=[
            changed(".github/workflows/supply-chain.yml"),
            changed("scripts/ci/check-floor-guard.py", "removed"),
        ], changed_files=2)
        self.run_gate(pol=RecordingPolicy())
        for route in self.api.calls:
            self.assertNotIn("contents", route)
            self.assertNotIn("blobs", route)
            self.assertNotIn(HEAD_SHA, route)


class PolicyDecisions(Harness):
    def test_error_findings_fail_with_bounded_constant_messages(self):
        marker = "TITLE-SECRET-MARKER"
        self.setup(files=[changed(".github/workflows/base-attestation.yml")],
                   title=f"fix(gate): {marker}")
        messages = set()
        for pol in (
            RecordingPolicy(findings=[Finding("error", marker, marker * 40)]),
            RecordingPolicy(attestation=[Finding("error", marker, marker * 40)]),
            RecordingPolicy(findings=[Finding("warning", "w", "w"),
                                      Finding("error", marker, marker)]),
        ):
            with self.assertRaises(gate.GateError) as ctx:
                self.run_gate(pol=pol)
            text = str(ctx.exception)
            self.assertLessEqual(len(text), 300)
            self.assertNotIn(marker, text)
            self.assertNotIn(self.pr["body"].strip()[:20], text)
            messages.add(text)
        self.assertEqual(len(messages), 1, messages)

    def test_supplement_added_only_when_the_gate_itself_is_touched(self):
        supply = ".github/workflows/supply-chain.yml"
        for touched in (
            ".github/workflows/base-attestation.yml",
            ".github/scripts/base_attestation_gate.py",
            "scripts/ci/test_base_attestation_gate.py",
        ):
            with self.subTest(touched=touched):
                self.setup(files=[changed(touched)])
                self.run_gate()
                self.assertEqual(len(self.pol.attested), 1)
                body, paths = self.pol.attested[0]
                self.assertEqual(body, self.pr["body"])
                self.assertIn(supply, paths)
        self.setup(files=[changed("src/lib.rs"), changed("docs/guide.md")], changed_files=2)
        self.run_gate()
        self.assertEqual(self.pol.attested, [])

    def test_indirect_stored_release_note_body_is_refused(self):
        self.setup(body="This release is too large to preview in the pull request body.\n"
                        "See the stored notes.")
        self.assertRejected()
        self.assertEqual(self.pol.envs, [])

    def test_supplement_applies_to_a_rename_away_from_the_gate(self):
        self.setup(files=[changed("docs/gone.md", "renamed",
                                  previous_filename=".github/scripts/base_attestation_gate.py")])
        self.run_gate()
        self.assertTrue(any(".github/workflows/supply-chain.yml" in p
                            for _, p in self.pol.attested))


class RealPolicy(Harness):
    """The gate against the real base `pr_standards.py`."""

    def go(self, files, body=PUBLIC_BODY, **over):
        self.setup(files=files, body=body, **over)
        return self.run_gate(pol=policy)

    def test_non_t0_pr_passes_without_attestation(self):
        self.assertEqual(self.go([changed("src/lib.rs")])["file_count"], 1)

    def test_t0_path_without_attestation_is_rejected(self):
        self.setup(files=[changed("crates/cutover/src/lib.rs")])
        self.assertRejected(pol=policy)

    def test_t0_path_with_valid_attestation_passes(self):
        receipt = self.go([changed("crates/cutover/src/lib.rs")],
                          body=PUBLIC_BODY + FULL_ATTESTATION)
        self.assertEqual(receipt["base_sha"], BASE_SHA)

    def test_t0_attestation_with_placeholder_or_unchecked_item_is_rejected(self):
        for bad in (
            FULL_ATTESTATION.replace("- [x] D3:", "- [ ] D3:"),
            FULL_ATTESTATION.replace("no secret sinks; redacted evidence only", "<secret sinks, ...>"),
            FULL_ATTESTATION.replace("risk_tier: T0", "risk_tier: T2"),
            FULL_ATTESTATION.replace("### Claims", "### Notes"),
        ):
            with self.subTest(bad=bad[-120:]):
                self.setup(files=[changed("migrations/0001.sql", "added")],
                           body=PUBLIC_BODY + bad)
                self.assertRejected(pol=policy)

    def test_rename_away_from_t0_path_needs_attestation(self):
        files = [changed("docs/notes.md", "renamed",
                         previous_filename="crates/cutover/src/lib.rs")]
        self.setup(files=files)
        self.assertRejected(pol=policy)
        self.assertEqual(self.go(files, body=PUBLIC_BODY + FULL_ATTESTATION)["path_count"], 2)

    def test_rename_into_t0_path_needs_attestation(self):
        files = [changed("crates/cutover/src/lib.rs", "renamed",
                         previous_filename="docs/notes.md")]
        self.setup(files=files)
        self.assertRejected(pol=policy)

    def test_removed_t0_file_needs_attestation(self):
        self.setup(files=[changed(".github/workflows/deploy-production.yml", "removed")])
        self.assertRejected(pol=policy)

    def test_self_gate_t0_supplement_for_new_gate_files(self):
        for touched in (
            ".github/workflows/base-attestation.yml",
            ".github/scripts/base_attestation_gate.py",
            "scripts/ci/test_base_attestation_gate.py",
        ):
            with self.subTest(touched=touched):
                self.setup(files=[changed(touched, "added")])
                self.assertRejected(pol=policy)
                self.assertEqual(
                    self.go([changed(touched, "added")],
                            body=PUBLIC_BODY + FULL_ATTESTATION)["file_count"],
                    1,
                )

    def test_hostile_head_workflow_changes_cannot_skip_the_base_judgment(self):
        # The head may skip, neuter or rewrite any workflow, including the one
        # that runs this gate. The verdict comes only from event metadata, the
        # API inventory and the base policy, so touching them is itself T0.
        files = [
            changed(".github/workflows/base-attestation.yml", "removed"),
            changed(".github/workflows/supply-chain.yml", "modified"),
            changed(".github/scripts/pr_standards.py", "modified"),
            changed(".github/scripts/base_attestation_gate.py", "modified"),
        ]
        self.setup(files=files, changed_files=4)
        self.assertRejected(pol=policy)
        self.assertEqual(set(self.api.calls) - {PR_ROUTE, REF_ROUTE},
                         {f"repos/{REPO}/pulls/{NUMBER}/files?per_page=100&page=1"})
        self.assertEqual(self.go(files, body=PUBLIC_BODY + FULL_ATTESTATION)["file_count"], 4)

    def test_hostile_body_cannot_forge_a_different_head_snapshot(self):
        body = PUBLIC_BODY + FULL_ATTESTATION + f"\nhead_sha: {OTHER_SHA}\n"
        receipt = self.go([changed("crates/cutover/src/lib.rs")], body=body)
        self.assertEqual(receipt["head_sha"], HEAD_SHA)


class Source(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.gate_src = GATE_PATH.read_text(encoding="utf-8")
        cls.wf = WORKFLOW_PATH.read_text(encoding="utf-8")

    def test_validate_imports_nothing_and_executes_nothing(self):
        src = inspect.getsource(gate.validate)
        self.assertNotRegex(src, r"^\s*(?:import|from)\s+\w", "validate must import nothing")
        self.assertNotIn("__import__", src)
        self.assertTrue(issubclass(gate.GateError, Exception))

    def test_gate_has_no_write_methods_or_code_execution(self):
        self.assertNotRegex(self.gate_src, r"""['"](?:POST|PUT|PATCH|DELETE)['"]""")
        for banned in ("subprocess", "os.system", "eval(", "exec(", "git/blobs", "/contents/",
                       "raw.githubusercontent"):
            self.assertNotIn(banned, self.gate_src)

    def test_gate_fixed_routes_are_documented_in_source(self):
        for fragment in ("git/ref/heads/", "per_page=100", "/files"):
            self.assertIn(fragment, self.gate_src)

    def test_gate_docs_state_required_workflow_binding_without_runtime_claim(self):
        doc = (inspect.getdoc(gate) or "") + "\n" + "\n".join(
            line for line in self.wf.splitlines() if line.lstrip().startswith("#")
        )
        lowered = doc.lower()
        self.assertRegex(lowered, r"required[- ]workflow binding")
        self.assertIn("not a custom pr-head status", lowered)
        self.assertIn("same-name", lowered)
        self.assertNotIn("runtime enforce", lowered)
        self.assertNotRegex(lowered, r"\bautomatically enforces\b")

    def test_check_name_is_distinct_from_existing_checks(self):
        def contexts(text):
            # Workflow name and job display name: what a check context shows.
            found = set(re.findall(r"^name:\s*(.+?)\s*$", text, re.M))
            found |= set(re.findall(r"^    name:\s*(.+?)\s*$", text, re.M))
            return {v.strip("'\"") for v in found}

        mine = contexts(self.wf)
        self.assertGreaterEqual(len(mine), 2, "workflow and job both need explicit names")
        others = set()
        for path in WORKFLOWS_DIR.glob("*.yml"):
            if path.name == WORKFLOW_PATH.name:
                continue
            text = path.read_text(encoding="utf-8")
            others |= contexts(text)
            others |= set(re.findall(r"^  ([A-Za-z0-9_-]+):\s*$",
                                     text.split("\njobs:", 1)[-1], re.M))
        self.assertFalse(mine & others, mine & others)
        self.assertNotIn("pr-lint", mine | set(self.job_ids()))

    def job_ids(self):
        jobs = self.wf.split("\njobs:", 1)
        self.assertEqual(len(jobs), 2, "workflow needs a jobs: section")
        return re.findall(r"^  ([A-Za-z0-9_-]+):\s*$", jobs[1], re.M)

    def test_workflow_trigger(self):
        self.assertRegex(self.wf, r"(?m)^\s*pull_request_target:", )
        self.assertNotRegex(self.wf, r"(?m)^\s*pull_request:", )
        for banned in ("workflow_run", "workflow_dispatch", "schedule:", "issue_comment"):
            self.assertNotIn(banned, self.wf)
        m = re.search(r"pull_request_target:\s*\n\s*types:\s*(.+)", self.wf)
        self.assertIsNotNone(m, "types: must follow pull_request_target")
        listed = set(re.findall(r"[a-z_]+", m.group(1)))
        self.assertEqual(listed, {"opened", "edited", "synchronize", "reopened",
                                  "ready_for_review"})

    def test_required_and_ordinary_runs_cannot_cancel_or_replace_each_other(self):
        self.assertNotRegex(self.wf, r"(?m)^\s*concurrency:")
        self.assertNotIn("cancel-in-progress", self.wf)

    def test_workflow_permissions_are_read_only(self):
        self.assertRegex(self.wf, r"(?m)^permissions:\s*\{\}\s*$")
        blocks = re.findall(r"^([ ]*)permissions:[ ]*\n((?:\1[ ]+\S[^\n]*\n)+)", self.wf, re.M)
        self.assertTrue(blocks, "explicit job permissions block required")
        for _, block in blocks:
            granted = dict(
                (k.strip(), v.strip())
                for k, v in (line.split(":", 1) for line in block.strip().splitlines())
            )
            self.assertEqual(granted, {"contents": "read", "pull-requests": "read"})
        self.assertNotRegex(self.wf, r":\s*write\b")
        self.assertNotIn("write-all", self.wf)

    def test_workflow_uses_no_secrets_but_the_github_token(self):
        self.assertNotIn("secrets.", self.wf)
        self.assertIn("github.token", self.wf)
        refs = set(re.findall(r"\$\{\{\s*([^}]+?)\s*\}\}", self.wf))
        self.assertLessEqual(refs, {
            "github.token",
            "github.event.pull_request.base.sha",
            "github.event.pull_request.number",
        })

    def test_single_unconditional_job_with_timeout(self):
        self.assertEqual(len(self.job_ids()), 1)
        self.assertNotRegex(self.wf, r"(?m)^\s+if:", )
        self.assertRegex(self.wf, r"(?m)^\s+runs-on:\s*ubuntu-latest\s*$")
        self.assertRegex(self.wf, r"(?m)^\s+timeout-minutes:\s*5\s*$")
        self.assertNotIn("continue-on-error", self.wf)
        self.assertNotIn("strategy:", self.wf)

    def test_checkout_is_pinned_base_sha_sparse_without_credentials(self):
        uses = re.findall(r"^\s*-?\s*uses:\s*(\S+)", self.wf, re.M)
        self.assertEqual(len(uses), 1, uses)
        self.assertRegex(uses[0], r"^actions/checkout@[0-9a-f]{40}$")
        self.assertRegex(self.wf, r"ref:\s*\$\{\{\s*github\.event\.pull_request\.base\.sha\s*\}\}")
        self.assertRegex(self.wf, r"persist-credentials:\s*false")
        at = self.wf.index("sparse-checkout:")
        window = self.wf[at : at + 80]
        self.assertIn(".github/scripts", window)
        self.assertNotIn(".github/workflows", window)

    def test_workflow_never_touches_head_content(self):
        for banned in ("pull_request.head", "head_ref", "head.sha", "head.ref",
                       "refs/pull/", "gh pr checkout", "git fetch", "npm ", "pip install",
                       "cargo ", "docker ", "curl ", "wget "):
            self.assertNotIn(banned, self.wf)

    def test_run_step_invokes_isolated_absolute_helper_with_event_file(self):
        run = re.search(r"python3\s+-I\s+(\S+)([^\n]*)", self.wf)
        self.assertIsNotNone(run)
        script, rest = run.groups()
        script = script.strip("\"'")
        self.assertTrue(script.endswith(".github/scripts/base_attestation_gate.py"), script)
        self.assertNotIn("..", script)
        self.assertIn("--event", rest)
        self.assertRegex(self.wf, r"GH_TOKEN:\s*\$\{\{\s*github\.token\s*\}\}")


class HttpBoundary(unittest.TestCase):
    def test_redirects_are_refused_without_forwarding_credentials(self):
        with self.assertRaises(gate.GateError):
            gate.NoRedirect().redirect_request(
                None, None, 302, "redirect", {}, "https://example.invalid/")

    def test_reader_uses_only_get_with_timeout_and_bounded_read(self):
        with mock.patch.object(gate, "build_opener"):
            reader = gate.RestReader(REPO, "synthetic-token")
            response = reader.opener.open.return_value.__enter__.return_value
            response.read.return_value = b'{"number":7}'
            self.assertEqual(reader(PR_ROUTE), {"number": 7})
            request = reader.opener.open.call_args.args[0]
            self.assertEqual(request.get_method(), "GET")
            self.assertEqual(request.full_url, f"https://api.github.com/{PR_ROUTE}")
            self.assertEqual(reader.opener.open.call_args.kwargs, {"timeout": 30})
            response.read.assert_called_once_with(gate.MAX_RESPONSE_BYTES + 1)
            for route in ("https://example.invalid/", f"repos/Other/repo/pulls/7",
                          f"repos/{REPO}/../other", f"{PR_ROUTE}#suffix"):
                with self.subTest(route=route), self.assertRaises(gate.GateError):
                    reader(route)
            self.assertEqual(reader.opener.open.call_count, 1)

    def test_oversized_and_malformed_responses_fail_with_constant_diagnostics(self):
        with mock.patch.object(gate, "build_opener"), mock.patch.object(
                gate, "MAX_RESPONSE_BYTES", 8):
            reader = gate.RestReader(REPO, "synthetic-token")
            response = reader.opener.open.return_value.__enter__.return_value
            for payload in (b"x" * 9, b"not-json"):
                with self.subTest(payload=payload), self.assertRaises(gate.GateError) as caught:
                    response.read.return_value = payload
                    reader(PR_ROUTE)
                self.assertNotIn("synthetic-token", str(caught.exception))
                self.assertNotIn(payload.decode(), str(caught.exception))


class Cli(unittest.TestCase):
    """Entry-point refusals that fire before any network call."""

    def run_cli(self, env_over, event=None, args=None):
        with tempfile.TemporaryDirectory() as tmp:
            event_path = Path(tmp) / "event.json"
            event_path.write_text(json.dumps(event if event is not None else
                                             event_for(pull_request())))
            env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                   "GITHUB_REPOSITORY": REPO,
                   "GITHUB_SHA": BASE_SHA,
                   "GITHUB_EVENT_NAME": "pull_request_target",
                   "GH_TOKEN": "x"}
            env.update(env_over)
            env = {k: v for k, v in env.items() if v is not None}
            argv = args if args is not None else ["--event", str(event_path)]
            return subprocess.run(
                [sys.executable, "-I", str(GATE_PATH), *argv],
                env=env, capture_output=True, text=True, timeout=30, cwd=tmp,
            )

    def test_wrong_event_name_is_refused(self):
        for name in ("pull_request", "push", "workflow_run", ""):
            with self.subTest(name=name):
                done = self.run_cli({"GITHUB_EVENT_NAME": name})
                self.assertNotEqual(done.returncode, 0)

    def test_missing_environment_is_refused(self):
        for key in ("GITHUB_REPOSITORY", "GITHUB_SHA", "GH_TOKEN"):
            with self.subTest(key=key):
                done = self.run_cli({key: None})
                self.assertNotEqual(done.returncode, 0)

    def test_missing_or_bad_event_file_is_refused(self):
        self.assertNotEqual(self.run_cli({}, args=[]).returncode, 0)
        self.assertNotEqual(self.run_cli({}, args=["--event", "/nonexistent/event.json"]).returncode, 0)
        self.assertNotEqual(self.run_cli({}, event=[1, 2]).returncode, 0)

    def test_token_is_never_echoed(self):
        marker = "x" * 32
        done = self.run_cli({"GITHUB_EVENT_NAME": "push", "GH_TOKEN": marker})
        self.assertNotIn(marker, done.stdout + done.stderr)


if __name__ == "__main__":
    unittest.main()
