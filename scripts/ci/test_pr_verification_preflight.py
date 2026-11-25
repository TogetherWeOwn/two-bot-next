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
        for verify in (f"- Ran the fixture. {run_link}\r\n{command}", f"- `{command}` passed."):
            with self.subTest(verify=verify), TemporaryDirectory() as directory, chdir(directory):
                git("init", "-q", "-b", "test/verification")
                git("config", "user.name", "Fixture")
                git("config", "user.email", "fixture@example.com")
                git("commit", "-q", "--allow-empty", "-m", "Base")
                base = git("rev-parse", "HEAD")
                source = Path("test_fixture.py")
                source.write_text("def test_present(): pass\n", encoding="utf-8")
                git("add", str(source))
                git("commit", "-qm", "Add test")
                head = git("rev-parse", "HEAD")
                source.write_text("def test_only_in_worktree(): pass\n", encoding="utf-8")

                draft = Path("draft.md")
                body = f"## Verification\r\n\r\n{verify}\r\n"
                draft.write_bytes(body.encode())
                rejected = run_link in body

                def validate(policy, text, files, read):
                    self.assertEqual(text, body)
                    self.assertEqual(files, [{"filename": str(source), "status": "added"}])
                    self.assertEqual(list(evidence.named_tests(str(source), read(str(source)))), ["test_present"])
                    draft.write_text("A later edit", encoding="utf-8")
                    if rejected:
                        raise evidence.EvidenceError("Actions URLs can go stale.")

                title = "test(ci): verify a synthetic draft"
                policy = Mock()
                with (patch.dict(sys.modules, {"pr_attestation": policy}),
                      patch.object(evidence, "validate", side_effect=validate) as validate_call,
                      patch.object(preflight, "check_squash", return_value=int(rejected)) as squash,
                      redirect_stdout(StringIO()) as stdout, redirect_stderr(StringIO()) as stderr):
                    result = preflight.main(["--title", title, "--body-file", str(draft),
                                             "--base", base, "--pr-number", "12"])
                self.assertEqual(result, int(rejected))
                validate_call.assert_called_once()
                self.assertIs(validate_call.call_args.args[0], policy)
                squash.assert_called_once_with(title, body, "test/verification", "12")
                self.assertIn("PR conventions OK", stdout.getvalue())
                if rejected:
                    self.assertIn("Actions URLs can go stale", stderr.getvalue())
                else:
                    self.assertIn(f"passed for {head} against {base}", stdout.getvalue())
