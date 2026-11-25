"""Focused coverage of the local preflight's draft and Git inputs."""

from contextlib import chdir, redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, patch

import pr_verification_preflight as preflight


class VerificationPreflightTests(unittest.TestCase):
    def test_squash_guard_receives_the_exact_body_file(self):
        body = "## Verification\r\n- Fixture passed.\r\n"

        def run(argv, *, env, timeout, check):
            self.assertEqual(argv, ["node", str(preflight.SCRIPTS / "commit-parse-guard.cjs"), "pr"])
            self.assertEqual((env["TITLE"], env["HEAD_REF"], env["PR_NUMBER"]),
                             ("test(ci): fixture", "test/verification", "12"))
            self.assertEqual(Path(env["BODY_FILE"]).read_bytes(), body.encode())
            return subprocess.CompletedProcess(argv, 1)

        with patch.object(preflight.subprocess, "run", side_effect=run):
            self.assertEqual(preflight.check_squash("test(ci): fixture", body, "test/verification", "12"), 1)

    def test_checks_share_draft_snapshot_and_use_committed_test(self):
        def git(*args):
            return subprocess.check_output(["git", *args], text=True).strip()

        evidence = preflight.pr_evidence
        run_link = "https://github.com/example/project/actions/runs/12/job/34"
        command = "rewrite(call(value))"
        with TemporaryDirectory() as directory, chdir(directory):
            git("init", "-q", "-b", "test/verification")
            git("config", "user.name", "Fixture")
            git("config", "user.email", "fixture@example.com")
            git("config", "commit.gpgSign", "false")
            standards = Path(".github/scripts/pr_standards.py")
            standards.parent.mkdir(parents=True)
            standards.write_text(
                "def main(env):\n"
                "    print('Base PR standards ran')\n"
                "    return int(env['TITLE'].endswith('standards failure'))\n",
                encoding="utf-8",
            )
            git("add", str(standards))
            git("commit", "-qm", "Base")
            base = git("rev-parse", "HEAD")
            source = Path("test_fixture.py")
            source.write_text("def test_present(): pass\n", encoding="utf-8")
            standards.write_text("def main(env): return 0\n", encoding="utf-8")
            git("add", str(source), str(standards))
            git("commit", "-qm", "Add test and change proposed standards")
            head = git("rev-parse", "HEAD")
            source.write_text("def test_only_in_worktree(): pass\n", encoding="utf-8")

            for failure in (None, "standards", "evidence", "squash"):
                with self.subTest(failure=failure):
                    title = f"test(ci): verify a synthetic draft with {failure} failure"
                    verification = {
                        "evidence": f"- Ran the fixture. {run_link}",
                        "squash": f"- Ran the fixture.\r\n{command}",
                    }.get(failure, f"- `{command}` passed.")
                    draft = Path("draft.md")
                    body = f"## Verification\r\n\r\n{verification}\r\n"
                    draft.write_bytes(body.encode())

                    def validate(policy, text, files, read):
                        self.assertEqual(text, body)
                        self.assertEqual(files, [
                            {"filename": str(standards), "status": "modified"},
                            {"filename": str(source), "status": "added"},
                        ])
                        self.assertEqual(list(evidence.named_tests(str(source), read(str(source)))), ["test_present"])
                        draft.write_text("A later edit", encoding="utf-8")
                        if failure == "evidence":
                            raise evidence.EvidenceError("Actions URLs can go stale.")

                    policy = Mock()
                    with (patch.dict(sys.modules, {"pr_attestation": policy}),
                          patch.object(evidence, "validate", side_effect=validate) as validate_call,
                          patch.object(preflight, "check_squash", return_value=int(failure == "squash")) as squash,
                          redirect_stdout(StringIO()) as stdout, redirect_stderr(StringIO()) as stderr):
                        result = preflight.main(["--title", title, "--body-file", str(draft),
                                                 "--base", base, "--pr-number", "12"])
                    self.assertEqual(result, int(failure is not None))
                    validate_call.assert_called_once()
                    self.assertIs(validate_call.call_args.args[0], policy)
                    squash.assert_called_once_with(title, body, "test/verification", "12")
                    self.assertIn("Base PR standards ran", stdout.getvalue())
                    if failure == "evidence":
                        self.assertIn("Actions URLs can go stale", stderr.getvalue())
                    if failure is None:
                        self.assertIn(f"passed for {head} against {base}", stdout.getvalue())
