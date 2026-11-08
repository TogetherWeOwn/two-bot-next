"""Exercise the workflow's actual inline Python without GitHub credentials.

The resolve step stays inline; the check step bridges the resolved body file
into .github/scripts/pr_standards.py, whose own unit tests live beside it."""

import base64
import contextlib
import copy
import io
import json
import os
import sys
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
# Fake internal tracker IDs for the warning tests. Built from parts so this
# public file carries no literal ID of its own.
TOG = "TO" + "G"
PAP = "PA" + "P"


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
CHECK = inline_script("Check title, body, branch and commits")
LIST_FILES = inline_script("List changed files")
TITLE = "chore(main): release 0.2.0"
BODY = "## Summary\n\nRelease the workspace with synchronized versions.\nPR_EOF\nauthor=dependabot[bot]\n\nFixes #12.\n"
OVERFLOW_SENTENCE = "This release is too large to preview in the pull request body. View the full release notes here:"
PR_HEAD = "release-please--branches--main--components--two-bot-next"
STORED_NOTES = f"## 0.2.0\n\n### Added\n\n* generated feature\n\n---\nFixes #12.\n"
OVERFLOW_LINK = f"{OVERFLOW_SENTENCE} https://github.com/TogetherWeOwn/two-bot-next/blob/{PR_HEAD}--release-notes/release-notes.md"
# A neutered working-tree gate: no attestation findings, always passes. The
# check step must never import this copy: it always judges PRs with the
# base-branch copy, so staging it proves which copy the step loads (a step
# that imported the head copy would pass a T0 gate edit with no attestation).
WEAKENED_HEAD_GATE = (
    "def attestation_findings(body, paths):\n"
    "    return []\n"
    "\n"
    "def main(env=None):\n"
    "    print('PR conventions OK')\n"
    "    return 0\n"
)
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

    def validate(self, metadata, success=True, base_gate=True, head_gate=None):
        metadata = dict(metadata)
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            if "body" in metadata:
                # CHECK reads the body from a file, never env: stage overrides
                # through a file so the validation path stays identical to CI.
                path = Path(tmp) / "body.md"
                path.write_text(metadata.pop("body"), encoding="utf-8")
                metadata["body_file"] = str(path)
            if "changed_files" in metadata:
                # CHECK reads the changed-file list from a file, never env.
                changed_path = Path(tmp) / "changed-files.txt"
                changed_path.write_text(metadata.pop("changed_files"), encoding="utf-8")
                metadata["changed_files_file"] = str(changed_path)
            # Mirror CI: the check step imports the gate from the base-branch
            # checkout (base-gate/), never the working tree. The base copy is
            # a symlink to this repo; a head_gate fixture stages a weakened
            # working-tree copy to prove the head copy is never imported.
            ws = Path(tmp) / "ws"
            ws.mkdir()
            if base_gate:
                (ws / "base-gate").symlink_to(ROOT)
            if head_gate is not None:
                scripts = ws / ".github" / "scripts"
                scripts.mkdir(parents=True)
                (scripts / "pr_standards.py").write_text(head_gate, encoding="utf-8")
            else:
                (ws / ".github").symlink_to(ROOT / ".github")
            env = {key.upper(): value for key, value in metadata.items()}
            # Mirror the check step's env in supply-chain.yml: this repo is
            # public, requires no card reference, and starts the new
            # body/reference rules in warn mode. The workspace is the staged
            # CI layout above, not the repo root.
            env.setdefault("REPO_PRIVATE", "false")
            env.setdefault("REQUIRE_CARD_REF", "false")
            env.setdefault("PR_STANDARDS_MODE", "warn")
            env["GITHUB_WORKSPACE"] = str(ws)
            if head_gate is not None:
                # Drop the cached gate so this exec really imports through the
                # staged layout: with the head copy weakened, only the base
                # copy fails the gate, proving which copy the step loads.
                sys.modules.pop("pr_standards", None)
            stdout = io.StringIO()
            with patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(stdout):
                if success:
                    try:
                        exec(CHECK, {})
                    except SystemExit as result:
                        # The script bridge exits 0 on success; a missing
                        # exit also means success.
                        self.assertEqual(result.code, 0)
                else:
                    with self.assertRaises(SystemExit) as result:
                        exec(CHECK, {})
                    self.assertEqual(result.exception.code, 1)
            return stdout.getvalue()

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
            env.setdefault("REPO_PRIVATE", "false")
            env.setdefault("REQUIRE_CARD_REF", "false")
            env.setdefault("PR_STANDARDS_MODE", "warn")
            # Mirror CI: the child imports the gate from the base-branch
            # checkout, never the working tree.
            ws = Path(tmp) / "ws"
            ws.mkdir()
            (ws / "base-gate").symlink_to(ROOT)
            (ws / ".github").symlink_to(ROOT / ".github")
            env["GITHUB_WORKSPACE"] = str(ws)
            for key, value in env.items():
                if key != "BODY_FILE":
                    self.assertLess(len(value), 4096, f"Child env {key} must stay small")
            script = Path(tmp) / "check_step.py"
            script.write_text(CHECK, encoding="utf-8")
            child_env = {k: v for k, v in os.environ.items() if k not in
                         ("TITLE", "BODY", "BODY_FILE", "EVENT", "AUTHOR", "HEAD_REF", "REPO_PRIVATE",
                          "REQUIRE_CARD_REF", "PR_STANDARDS_MODE", "GITHUB_WORKSPACE",
                          "COMMITS", "PR_NUMBER", "REPO")}
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

    def test_dispatch_overflow_without_card_ref_passes(self):
        # A public repo needs no card reference: stored notes pass on their own.
        metadata = self.resolve({**PR, "body": OVERFLOW_LINK}, stored_notes="## 0.2.0\n\nNo card reference here at all, just release notes text.\n")
        self.assertNotIn("Internal ID", self.validate(metadata))

    def test_dispatch_overflow_empty_stored_notes_fails_closed(self):
        with self.assertRaises(SystemExit):
            self.resolve({**PR, "body": OVERFLOW_LINK}, stored_notes="\n")

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

    def test_gate_edit_without_attestation_fails(self):
        # End to end through the real check step: a PR that edits the gate
        # with no attestation fails, judged by the base-branch copy.
        metadata = self.resolve(EVENT_NAME="pull_request", EVENT_HEAD_REF="fix/harden-gate",
                                EVENT_AUTHOR="contributor")
        output = self.validate({**metadata, "changed_files": ".github/scripts/pr_standards.py\n"},
                               success=False)
        self.assertIn("::error title=Attestation::", output)

    def test_missing_base_gate_fails_closed(self):
        # Without the base-branch checkout the step must fail, never fall back
        # to the PR's own copy.
        metadata = self.resolve()
        output = self.validate({**metadata}, success=False, base_gate=False)
        self.assertIn("Base-branch gate checkout is missing", output)

    def test_weakened_head_copy_is_ignored(self):
        # The working-tree gate is neutered, but the step still fails a T0
        # gate edit: it imports the base-branch copy, never the head copy.
        metadata = self.resolve(EVENT_NAME="pull_request", EVENT_HEAD_REF="fix/harden-gate",
                                EVENT_AUTHOR="contributor")
        output = self.validate(
            {**metadata, "changed_files": ".github/workflows/supply-chain.yml\n"},
            success=False, head_gate=WEAKENED_HEAD_GATE)
        self.assertIn("::error title=Attestation::", output)

    def test_delimiter_cannot_spoof_dependency_bot_exemption(self):
        self.validate(self.resolve({**copy.deepcopy(PR), "title": "Invalid title"}), success=False)

    def test_body_without_card_ref_passes_silently(self):
        body = "Long description of what changed and why, without a card reference."
        output = self.validate({**self.resolve(), "body": body})
        self.assertNotIn("::warning", output)
        self.assertNotIn("::error", output)

    def test_empty_body_still_fails(self):
        self.validate({**self.resolve(), "body": "<!-- nothing here -->\n\n## Summary\n"}, success=False)

    def test_internal_id_in_title_fails_in_warn_mode(self):
        # Tracker ids in public title/body always fail, even in warn mode.
        output = self.validate({**self.resolve(), "title": f"fix(auth): refuse expired sudo sessions ({TOG}-123)"}, success=False)
        self.assertIn(f"::error title=Internal reference::The PR title holds ticket id {TOG}-123", output)

    def test_internal_id_in_body_fails_in_warn_mode(self):
        # Tracker ids in public title/body always fail, even in warn mode.
        body = f"Long description of what changed and why.\n\nRefs: {PAP}-42 and {TOG}-7, {TOG}-7.\n"
        output = self.validate({**self.resolve(), "body": body}, success=False)
        self.assertIn(f"::error title=Internal reference::The PR body holds ticket id {PAP}-42", output)

    def test_template_placeholder_text_is_not_an_internal_id(self):
        body = ("## Checklist\n\n- [x] No secret, token, private URL, or internal card ID "
                f"({TOG}-, {PAP}-) is in the diff, the title, the body, the commits, or the branch name\n"
                f"Prefix lookalikes such as A{TOG}-12 or {TOG}-12abc stay quiet.\n")
        self.assertNotIn("Internal reference", self.validate({**self.resolve(), "body": body}))

    def test_main_commit_validation(self):
        self.validate({"event": "push", "commits": json.dumps([{"message": "fix(release): repair release validation"}])})
        self.validate({"event": "push", "commits": json.dumps([{"message": "Invalid commit"}])}, success=False)

    def test_main_commit_internal_id_warns_without_failing(self):
        commits = json.dumps([{"id": "0123456789abcdef", "message": f"fix(release): repair validation ({TOG}-5)\n\nbody"}])
        output = self.validate({"event": "push", "commits": commits})
        self.assertIn(f"::warning title=Internal reference::Commit 0123456789 holds ticket id {TOG}-5", output)


class BaseGateSnapshotTests(unittest.TestCase):
    def test_base_checkout_must_match_the_inventory_snapshot(self):
        script = inline_script("Verify base-branch gate snapshot")
        for expected, actual, success in (("b" * 40, "b" * 40, True),
                                          ("b" * 40, "c" * 40, False),
                                          ("", "b" * 40, False),
                                          ("main", "main", False)):
            with self.subTest(expected=expected, actual=actual):
                env = {"GITHUB_WORKSPACE": "/fixture", "EXPECTED_BASE_SHA": expected}
                with patch.dict(os.environ, env, clear=True), patch(
                        "subprocess.check_output", return_value=actual + "\n") as git:
                    if success:
                        exec(script, {})
                    else:
                        with self.assertRaises(SystemExit):
                            exec(script, {})
                if git.called:
                    self.assertEqual(git.call_args.args[0],
                                     ["git", "-C", "/fixture/base-gate", "rev-parse", "HEAD"])


class ChangedFileInventoryTests(unittest.TestCase):
    def inventory(self, entries, count=None, success=True, event="pull_request",
                  metadata_overrides=None, after_overrides=None, git_changes=None,
                  gitlink=None, **env_overrides):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            root = Path(tmp)
            metadata = {
                "state": "open", "changed_files": len(entries) if count is None else count,
                "head": {"sha": "a" * 40, "ref": "fix/inventory",
                         "repo": {"full_name": "TogetherWeOwn/two-bot-next"}},
                "base": {"sha": "b" * 40, "ref": "main"},
            }
            if git_changes is not None:
                repo = root / "pr-diff"
                repo.mkdir()
                def git(*args):
                    return subprocess.check_output(["git", "-C", str(repo), *args],
                                                   text=True, stderr=subprocess.PIPE).strip()
                git("init", "--quiet")
                git("config", "user.name", "Fixture")
                git("config", "user.email", "fixture@example.invalid")
                for index, changes in enumerate(git_changes):
                    for name, content in changes.items():
                        path = repo / name
                        if content is None:
                            path.unlink()
                        else:
                            path.parent.mkdir(parents=True, exist_ok=True)
                            path.write_text(content)
                    git("add", "--all")
                    if index == 1 and gitlink:
                        git("update-index", "--add", "--cacheinfo",
                            f"160000,{git('rev-parse', 'HEAD')},{gitlink}")
                    git("commit", "--quiet", "--allow-empty", "-m", "fixture")
                metadata["base"]["sha"] = git("rev-parse", "HEAD~1")
                metadata["head"]["sha"] = git("rev-parse", "HEAD")
            metadata.update(metadata_overrides or {})
            after = {**metadata, **(after_overrides or {})}
            entries = [{"status": "modified", **entry} for entry in entries]
            fixture = root / "fixture.json"
            fixture.write_text(json.dumps({"before": metadata, "after": after,
                                           "pages": [entries[i:i + 100] for i in range(0, len(entries), 100)] or [[]]}))
            # Only gh is stubbed. The actual inline step and (on dispatch) git
            # run as child processes; no GitHub credentials or network are used.
            gh = root / "gh"
            gh.write_text("#!/usr/bin/python3\n" + textwrap.dedent('''\
                import json, pathlib, sys
                root = pathlib.Path(__file__).parent
                fixture = json.loads((root / "fixture.json").read_text())
                with (root / "calls").open("a") as calls:
                    calls.write(json.dumps(sys.argv[1:]) + "\\n")
                if "/files?" in sys.argv[2]:
                    if "--slurp" in sys.argv:
                        print(json.dumps(fixture["pages"]))
                    else:
                        for page in fixture["pages"]:
                            for entry in page:
                                for key in ("filename", "previous_filename"):
                                    if entry.get(key):
                                        print(entry[key])
                else:
                    seen = root / "seen"
                    print(json.dumps(fixture["after" if seen.exists() else "before"]))
                    seen.touch()
                '''))
            gh.chmod(0o755)
            output = root / "output"
            output.touch()
            env = {"PATH": f"{root}:{os.environ['PATH']}", "PR_NUMBER": "42",
                   "GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next", "EVENT_NAME": event,
                   "EVENT_HEAD_SHA": metadata["head"]["sha"],
                   "EVENT_BASE_SHA": metadata["base"]["sha"],
                   "GITHUB_SHA": metadata["head"]["sha"],
                   "GITHUB_REF": "refs/heads/fix/inventory", "GITHUB_WORKSPACE": tmp,
                   "GITHUB_OUTPUT": str(output), "RUNNER_TEMP": tmp, **env_overrides}
            script = root / "list-step"
            script.write_text(LIST_FILES)
            command = ([sys.executable, "-I"] if "import json" in LIST_FILES else ["bash"])
            result = subprocess.run([*command, str(script)], env=env, capture_output=True, text=True)
            if not success:
                self.assertNotEqual(result.returncode, 0, "Incomplete inventory passed the gate")
                self.assertEqual(output.read_text(), "", "Failure must not publish an inventory")
                return result.stderr
            self.assertEqual(result.returncode, 0, result.stderr)
            emitted = dict(line.split("=", 1) for line in output.read_text().splitlines())
            paths = Path(emitted["changed_files_file"]).read_text().splitlines()
            calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
            return paths, emitted, calls

    def test_truncated_files_fail_closed(self):
        entries = [{"filename": f"docs/page-{n}.md"} for n in range(3000)]
        self.inventory(entries, count=3001, success=False)

    def test_complete_normal_and_exact_api_limit(self):
        for count in (1, 101, 3000):
            with self.subTest(count=count):
                entries = [{"filename": f"docs/page-{n}.md"} for n in range(count)]
                paths, _, calls = self.inventory(entries)
                self.assertEqual(paths, [entry["filename"] for entry in entries])
                self.assertIn("--paginate", calls[1])
                self.assertIn("--slurp", calls[1])

    def test_entry_count_precedes_rename_expansion(self):
        entries = [{"filename": "docs/new.md", "previous_filename": "auth/old.rs", "status": "renamed"},
                   {"filename": "docs/other.md", "status": "removed"}]
        paths, _, _ = self.inventory(entries)
        self.assertEqual(paths, ["docs/new.md", "auth/old.rs", "docs/other.md"])
        self.inventory(entries, count=3, success=False)

    def test_empty_missing_invalid_and_inconsistent_counts_fail_closed(self):
        for entries, count in (([], 0), ([], 1), ([{"filename": "a.rs"}], 2),
                               ([{"filename": "a.rs"}], 0), ([{"filename": "a.rs"}], "1"),
                               ([{"filename": "a.rs"}], True), ([{"filename": "a.rs"}], -1)):
            with self.subTest(count=count, entries=entries):
                self.inventory(entries, count=count, success=False)
        self.inventory([{"filename": "a.rs"}], metadata_overrides={"changed_files": None}, success=False)

    def test_duplicate_or_unrepresentable_entries_fail_closed(self):
        for entries in ([{"filename": "a.rs"}] * 2, [{}], [{"filename": ""}],
                        [{"filename": "auth/a.rs\nother"}], [{"filename": 42}],
                        [{"filename": "new.rs", "previous_filename": "auth/old.rs\n"}]):
            with self.subTest(entries=entries):
                self.inventory(entries, success=False)

    def test_renamed_entries_require_their_source_path(self):
        for extra in ({}, {"previous_filename": None}, {"previous_filename": ""}):
            with self.subTest(extra=extra):
                self.inventory([{"filename": "docs/new.md", "status": "renamed", **extra}],
                               success=False)
        self.inventory([{"filename": "docs/new.md", "status": "unknown"}], success=False)

    def test_dispatch_includes_gitlinks_despite_submodule_ignore_policy(self):
        paths, _, _ = self.inventory(
            [], count=3, event="workflow_dispatch", gitlink="auth",
            git_changes=[{}, {"docs/page.md": "ordinary change", ".gitmodules":
                             '[submodule "fixture"]\n\tpath = auth\n'
                             '\turl = https://example.invalid/fixture\n\tignore = all\n'}])
        self.assertEqual(set(paths), {".gitmodules", "auth", "docs/page.md"})

    def test_stale_or_changing_inventory_fails_closed(self):
        entries = [{"filename": "a.rs"}]
        for overrides in ({"EVENT_HEAD_SHA": "c" * 40}, {"EVENT_BASE_SHA": "c" * 40}):
            with self.subTest(overrides=overrides):
                self.inventory(entries, success=False, **overrides)
        for after in ({"changed_files": 2}, {"head": {"sha": "c" * 40}},
                      {"base": {"sha": "c" * 40}}):
            with self.subTest(after=after):
                self.inventory(entries, after_overrides=after, success=False)

    def test_dispatch_uses_complete_pinned_diff_beyond_api_limit(self):
        changes = {f"docs/page-{n}.md": str(n) for n in range(3000)}
        changes["auth/policy.rs"] = "critical change"
        paths, emitted, calls = self.inventory(
            [], count=3001, event="workflow_dispatch", git_changes=[{}, changes])
        self.assertEqual(set(paths), set(changes))
        self.assertRegex(emitted["base_sha"], r"^[0-9a-f]{40}$")
        self.assertFalse(any("/files?" in call[1] for call in calls))
        # The complete diff still feeds the same base-branch T0 floor.
        metadata = PRLintTests().resolve(EVENT_NAME="pull_request", EVENT_HEAD_REF="fix/inventory",
                                         EVENT_AUTHOR="contributor")
        output = PRLintTests().validate({**metadata, "changed_files": "\n".join(paths)}, success=False)
        self.assertIn("::error title=Attestation::", output)

    def test_dispatch_diff_preserves_old_and_new_rename_paths(self):
        paths, _, _ = self.inventory(
            [], count=1, event="workflow_dispatch",
            git_changes=[{"auth/old.rs": "unchanged"},
                         {"auth/old.rs": None, "docs/new.md": "unchanged"}])
        self.assertEqual(set(paths), {"auth/old.rs", "docs/new.md"})

    def test_dispatch_rejects_stale_fork_closed_wrong_ref_or_missing_history(self):
        entries = [{"filename": "a.rs"}]
        for overrides in ({"GITHUB_SHA": "c" * 40}, {"GITHUB_REF": "refs/heads/other"}):
            with self.subTest(overrides=overrides):
                self.inventory(entries, event="workflow_dispatch", success=False, **overrides)
        for metadata in ({"state": "closed"}, {"head": {"sha": "a" * 40, "ref": "fix/inventory",
                                                          "repo": {"full_name": "other/fork"}}},
                         {"base": {"sha": "b" * 40, "ref": "other"}}):
            with self.subTest(metadata=metadata):
                self.inventory(entries, event="workflow_dispatch", metadata_overrides=metadata, success=False)
        self.inventory(entries, event="workflow_dispatch", success=False)
        self.inventory([], count=1, event="workflow_dispatch", git_changes=[{}, {}], success=False)


if __name__ == "__main__":
    unittest.main()
