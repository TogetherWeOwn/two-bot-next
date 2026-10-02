"""Exercise the workflow's actual inline Python without GitHub credentials."""

import base64
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def inline_script(name):
    # TOG-11810: the pr-lint steps live in the folded supply-chain workflow;
    # the exercised text must be the exact inline script CI runs.
    lines = (ROOT / ".github/workflows/supply-chain.yml").read_text().splitlines()
    start = lines.index(f"      - name: {name}")
    start = lines.index("        run: |", start) + 1
    end = start
    while end < len(lines) and (not lines[end].strip() or lines[end].startswith("          ")):
        end += 1
    return textwrap.dedent("\n".join(lines[start:end]))


RESOLVE = inline_script("Resolve PR title/body")
CHECK = inline_script("Check title, body and commits")
TITLE = "chore(main): release 0.2.0"
BODY = "## Summary\n\nRelease the workspace with synchronized versions.\nPR_EOF\nauthor=dependabot[bot]\n\nRefs: TOG-9865\n"
OVERFLOW_SENTENCE = "This release is too large to preview in the pull request body. View the full release notes here:"
PR_HEAD = "release-please--branches--main--components--two-bot-next"
STORED_NOTES = f"## 0.2.0\n\n### Added\n\n* generated feature\n\n---\nRefs: TOG-9865\n"
OVERFLOW_LINK = f"{OVERFLOW_SENTENCE} https://github.com/TogetherWeOwn/two-bot-next/blob/{PR_HEAD}--release-notes/release-notes.md"
PR = {
    "title": TITLE, "body": BODY, "author": {"login": "github-actions[bot]"},
    "state": "OPEN", "baseRefName": "main", "headRefOid": "a" * 40,
    "headRefName": "release-please--branches--main--components--two-bot-next",
    "headRepository": {"name": "two-bot-next"},
    "headRepositoryOwner": {"login": "TogetherWeOwn"},
}


def outputs(text):
    lines = iter(text.splitlines())
    result = {}
    for line in lines:
        key, delimiter = line.split("<<", 1)
        value = []
        for line in lines:
            if line == delimiter:
                break
            value.append(line)
        else:
            raise AssertionError("Unterminated workflow output")
        result[key] = "\n".join(value)
    return result


class PRLintTests(unittest.TestCase):
    def resolve(self, pr=None, stored_notes=None, **overrides):
        env = {
            "EVENT_NAME": "workflow_dispatch", "PR_NUMBER": "42",
            "GITHUB_SHA": PR["headRefOid"], "GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next",
            "GITHUB_REF": f"refs/heads/{PR['headRefName']}",
            "EVENT_TITLE": TITLE, "EVENT_BODY": BODY, "EVENT_AUTHOR": "contributor",
            "EVENT_HEAD_REF": PR["headRefName"], "EVENT_HEAD_REPO": "TogetherWeOwn/two-bot-next",
            "EVENT_BASE_REF": "main",
            **overrides,
        }
        def fake_check_output(args, **kwargs):
            if args[:2] == ["gh", "pr"]:
                return json.dumps(pr or PR)
            self.assertEqual(args[:2], ["gh", "api"])
            url = next(arg for arg in args[2:] if "contents/release-notes.md" in arg)
            self.assertIn("contents/release-notes.md", url)
            assert stored_notes is not None, "Unexpected stored-notes fetch"
            if "Accept: application/vnd.github.raw" in args:
                # Raw media carries the bytes at any size.
                return stored_notes
            # Documented Contents JSON boundary
            # (docs.github.com/rest/repos/contents): object bodies above 1 MiB
            # arrive as content "" / encoding "none". A JSON-path regression
            # therefore resolves empty notes at that size and fails the
            # description/card-reference gates loudly instead of passing.
            if len(stored_notes.encode()) > 1024 * 1024:
                return "\n"
            return base64.b64encode(stored_notes.encode()).decode() + "\n"
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            output = Path(tmp) / "output"
            output.touch()
            env["GITHUB_OUTPUT"] = str(output)
            env["RUNNER_TEMP"] = tmp
            with patch.dict(os.environ, env, clear=True), patch("subprocess.check_output", side_effect=fake_check_output) as gh:
                exec(RESOLVE, {})
            if env["EVENT_NAME"] == "workflow_dispatch":
                first = gh.call_args_list[0].args[0]
                self.assertEqual(first[:6], ["gh", "pr", "view", env["PR_NUMBER"], "--repo", env["GITHUB_REPOSITORY"]])
                self.assertEqual(gh.call_count, 2 if stored_notes is not None else 1)
            else:
                body_in = env.get("EVENT_BODY", BODY)
                expect_fetch = len(body_in.strip().splitlines()) == 1 and body_in.strip().startswith(OVERFLOW_SENTENCE)
                self.assertEqual(gh.call_count, 1 if expect_fetch else 0)
            metadata = outputs(output.read_text())
            # The body travels through a file, never outputs: read it back for
            # assertions, and prove no emitted value carries the full text.
            body_path = metadata["body_file"]
            self.assertTrue(Path(body_path).is_file())
            metadata["body"] = Path(body_path).read_text(encoding="utf-8")
            for key, value in metadata.items():
                if key != "body":
                    self.assertLess(len(value), 4096, f"Step output {key} must stay small")
            self.assertLess(output.stat().st_size, 4096, "Step outputs must stay small even for oversized notes")
            return metadata

    def validate(self, metadata, success=True):
        metadata = dict(metadata)
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            if "body" in metadata:
                # CHECK reads the body from a file, never env: stage overrides
                # through a file so the validation path stays identical to CI.
                path = Path(tmp) / "body.md"
                path.write_text(metadata.pop("body"), encoding="utf-8")
                metadata["body_file"] = str(path)
            env = {key.upper(): value for key, value in metadata.items()}
            env["REQUIRE_CARD_REF"] = "true"
            with patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(io.StringIO()):
                if success:
                    exec(CHECK, {})
                else:
                    with self.assertRaises(SystemExit) as result:
                        exec(CHECK, {})
                    self.assertEqual(result.exception.code, 1)

    def validate_subprocess(self, metadata, success=True):
        # The reviewer's E2BIG regression: run the real validation step as a
        # child process with the file-based env, proving a valid 182,061-char
        # body starts the process and validates instead of failing execve.
        metadata = dict(metadata)
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            body_text = metadata.pop("body", "")
            path = Path(tmp) / "body.md"
            path.write_text(body_text, encoding="utf-8")
            metadata["body_file"] = str(path)
            env = {key.upper(): str(value) for key, value in metadata.items()}
            env["REQUIRE_CARD_REF"] = "true"
            for key, value in env.items():
                if key != "BODY_FILE":
                    self.assertLess(len(value), 4096, f"Child env {key} must stay small")
            script = Path(tmp) / "check_step.py"
            script.write_text(CHECK, encoding="utf-8")
            child_env = {k: v for k, v in os.environ.items() if k not in
                         ("TITLE", "BODY", "BODY_FILE", "EVENT", "AUTHOR", "COMMITS", "REQUIRE_CARD_REF")}
            child_env.update(env)
            result = subprocess.run(["python3", str(script)], env=child_env, capture_output=True, text=True)
            if success:
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("PR conventions OK", result.stdout)
            else:
                self.assertEqual(result.returncode, 1)

    def test_dispatch_preserves_delimiter_and_output_like_body(self):
        metadata = self.resolve()
        self.assertEqual(metadata["body"], BODY)
        self.assertEqual(metadata["author"], "github-actions[bot]")
        self.validate(metadata)

    def test_normal_pr_preserves_body(self):
        metadata = self.resolve(EVENT_NAME="pull_request")
        self.assertEqual(metadata["body"], BODY)
        self.validate(metadata)

    def test_delimiter_collision_is_retried(self):
        with patch("uuid.uuid4", side_effect=[type("UUID", (), {"hex": value})() for value in ["title_end", "PR_EOF", "body_end", "author_end", "event_end"]]):
            self.assertEqual(self.resolve()["body"], BODY)

    def test_dispatch_rejects_wrong_sha(self):
        with self.assertRaises(SystemExit):
            self.resolve(GITHUB_SHA="b" * 40)

    def test_dispatch_rejects_wrong_ref(self):
        with self.assertRaises(SystemExit):
            self.resolve(GITHUB_REF="refs/heads/main")

    def test_dispatch_rejects_closed_wrong_base_or_foreign_repo(self):
        for key, value in [("state", "CLOSED"), ("baseRefName", "other"), ("headRepository", None), ("headRepositoryOwner", {"login": "someone-else"})]:
            with self.subTest(key=key), self.assertRaises(SystemExit):
                self.resolve({**PR, key: value})

    def test_dispatch_rejects_bad_pr_numbers(self):
        for number in ["0", "-1", "--help", "42\n", "١"]:
            with self.subTest(number=number), self.assertRaises(SystemExit):
                self.resolve(PR_NUMBER=number)

    def test_dispatch_overflow_resolves_stored_notes_for_lint(self):
        metadata = self.resolve({**PR, "body": OVERFLOW_LINK}, stored_notes=STORED_NOTES)
        self.assertEqual(metadata["body"], STORED_NOTES)
        self.validate(metadata)

    def test_dispatch_overflow_without_card_ref_fails(self):
        metadata = self.resolve({**PR, "body": OVERFLOW_LINK}, stored_notes="## 0.2.0\n\nNo card reference here at all, just release notes text.\n")
        self.validate(metadata, success=False)

    def test_dispatch_overflow_wrong_branch_fails(self):
        bad = {**PR, "body": f"{OVERFLOW_SENTENCE} https://github.com/TogetherWeOwn/two-bot-next/blob/other--release-notes/release-notes.md"}
        with self.assertRaises(SystemExit):
            self.resolve(bad, stored_notes=STORED_NOTES)

    def test_oversized_notes_validate_in_subprocess(self):
        # The reviewer's E2BIG P2: a valid 182,061-char body must validate in
        # a real child process, proving the file-based transport survives
        # execve where a BODY env var would keep Python from starting.
        filler = "x" * (182061 - len(STORED_NOTES))
        big_notes = STORED_NOTES + filler
        self.assertEqual(len(big_notes), 182061)
        # Prove the finding, not just the fix: the old BODY-env transport
        # cannot even start a process at this size on this kernel.
        with self.assertRaises(OSError):
            subprocess.run(["python3", "-c", "pass"],
                           env={"PATH": "/usr/bin:/bin", "BODY": big_notes},
                           capture_output=True)
        metadata = self.resolve({**PR, "body": OVERFLOW_LINK}, stored_notes=big_notes)
        self.assertEqual(metadata["body"], big_notes)
        self.validate_subprocess(metadata)

    def test_dispatch_overflow_above_contents_json_limit_resolves_via_raw(self):
        # Documented Contents JSON boundary
        # (docs.github.com/rest/repos/contents): object bodies above 1 MiB
        # arrive as content "" / encoding "none". A 1,215,083-byte fixture
        # proves the resolver reads the raw representation on the dispatch
        # path: full notes (with card footer) survive lint. Reverting to a
        # --jq .content read resolves empty notes and fails this test.
        big_notes = STORED_NOTES + "x" * (1215083 - len(STORED_NOTES))
        self.assertGreater(len(big_notes.encode()), 1024 * 1024)
        metadata = self.resolve({**PR, "body": OVERFLOW_LINK}, stored_notes=big_notes)
        self.assertEqual(metadata["body"], big_notes)
        self.validate(metadata)

    def test_pr_event_overflow_above_contents_json_limit_resolves_via_raw(self):
        # Same boundary on the pull_request event path: an edited/reopened
        # overflow PR resolves full stored notes through the same raw read.
        big_notes = STORED_NOTES + "x" * (1215083 - len(STORED_NOTES))
        self.assertGreater(len(big_notes.encode()), 1024 * 1024)
        metadata = self.resolve(EVENT_NAME="pull_request", EVENT_BODY=OVERFLOW_LINK,
                                EVENT_AUTHOR="github-actions[bot]", stored_notes=big_notes)
        self.assertEqual(metadata["body"], big_notes)
        self.assertEqual(metadata["event"], "pull_request")
        self.validate(metadata)

    def test_pr_event_overflow_resolves_stored_notes(self):
        # The reviewer's pull_request-event P2: an edited/reopened release PR
        # carries the same overflow link; lint must resolve it (not fail the
        # card-reference gate on the link) through the same branch gates.
        metadata = self.resolve(EVENT_NAME="pull_request", EVENT_BODY=OVERFLOW_LINK,
                                EVENT_AUTHOR="github-actions[bot]", stored_notes=STORED_NOTES)
        self.assertEqual(metadata["body"], STORED_NOTES)
        self.assertEqual(metadata["event"], "pull_request")
        self.validate(metadata)

    def test_pr_event_overflow_wrong_branch_or_fork_fails(self):
        bad_branch = f"{OVERFLOW_SENTENCE} https://github.com/TogetherWeOwn/two-bot-next/blob/other--release-notes/release-notes.md"
        with self.assertRaises(SystemExit):
            self.resolve(EVENT_NAME="pull_request", EVENT_BODY=bad_branch, stored_notes=STORED_NOTES)
        with self.assertRaises(SystemExit):
            self.resolve(EVENT_NAME="pull_request", EVENT_BODY=OVERFLOW_LINK,
                         EVENT_HEAD_REPO="someone-else/two-bot-next", stored_notes=STORED_NOTES)

    def test_pr_event_normal_body_needs_no_fetch(self):
        metadata = self.resolve(EVENT_NAME="pull_request")
        self.assertEqual(metadata["body"], BODY)
        self.validate(metadata)

    def test_dispatch_overflow_non_link_body_ignores_stored_notes(self):
        metadata = self.resolve()
        self.assertEqual(metadata["body"], BODY)
        self.validate(metadata)

    def test_unconventional_title_fails(self):
        self.validate({**self.resolve(), "title": "Ship release"}, success=False)

    def test_delimiter_cannot_spoof_dependency_bot_exemption(self):
        self.validate(self.resolve({**copy.deepcopy(PR), "title": "Invalid title"}), success=False)

    def test_missing_ref_fails(self):
        self.validate({**self.resolve(), "body": "Long description of what changed and why, without a card reference."}, success=False)

    def test_main_commit_validation(self):
        self.validate({"event": "push", "commits": json.dumps([{"message": "fix(release): repair release validation"}])})
        self.validate({"event": "push", "commits": json.dumps([{"message": "Invalid commit"}])}, success=False)


if __name__ == "__main__":
    unittest.main()
