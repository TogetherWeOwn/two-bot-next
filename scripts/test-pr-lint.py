"""Exercise the workflow's actual inline Python without GitHub credentials."""

import base64
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import tempfile
import textwrap
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def inline_script(name):
    lines = (ROOT / ".github/workflows/pr-lint.yml").read_text().splitlines()
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
            **overrides,
        }
        def fake_check_output(args, **kwargs):
            if args[:2] == ["gh", "pr"]:
                return json.dumps(pr or PR)
            self.assertEqual(args[:2], ["gh", "api"])
            self.assertIn("contents/release-notes.md", args[2])
            assert stored_notes is not None, "Unexpected stored-notes fetch"
            return base64.b64encode(stored_notes.encode()).decode() + "\n"
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            output = Path(tmp) / "output"
            output.touch()
            env["GITHUB_OUTPUT"] = str(output)
            with patch.dict(os.environ, env, clear=True), patch("subprocess.check_output", side_effect=fake_check_output) as gh:
                exec(RESOLVE, {})
            if env["EVENT_NAME"] == "workflow_dispatch":
                first = gh.call_args_list[0].args[0]
                self.assertEqual(first[:6], ["gh", "pr", "view", env["PR_NUMBER"], "--repo", env["GITHUB_REPOSITORY"]])
                self.assertEqual(gh.call_count, 2 if stored_notes is not None else 1)
            else:
                gh.assert_not_called()
            return outputs(output.read_text())

    def validate(self, metadata, success=True):
        env = {key.upper(): value for key, value in metadata.items()}
        env["REQUIRE_CARD_REF"] = "true"
        with patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(io.StringIO()):
            if success:
                exec(CHECK, {})
            else:
                with self.assertRaises(SystemExit) as result:
                    exec(CHECK, {})
                self.assertEqual(result.exception.code, 1)

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
