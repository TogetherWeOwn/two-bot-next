"""Run the actual reconciliation shell with local Git and a fail-closed gh mock."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
# Native release-please 17.6.0 derives this branch's component from the root
# package name (`two-bot-next`); the lifecycle fixture asserts the live
# library still generates this head.
BRANCH = "release-please--branches--main--components--two-bot-next"
NOTES_BRANCH = BRANCH + "--release-notes"
OVERFLOW_SENTENCE = "This release is too large to preview in the pull request body. View the full release notes here:"
OVERFLOW_BODY = f"{OVERFLOW_SENTENCE} https://github.com/fixture/repo/blob/{NOTES_BRANCH}/release-notes.md"
NOTES = "## 0.2.0\n\n### Added\n\n* generated feature"
BODY = f":robot: release\n---\n\n{NOTES}\n\n---\nRefs: TOG-9865\n"
CHANGELOG = f"# Changelog\n\n{NOTES}\n\n## Changelog\n\n## Unreleased\n\n### Fixed\n\n- historical RSVP repair\n"


def reconciliation_shell():
    lines = (ROOT / ".github/workflows/release.yml").read_text().splitlines()
    start = lines.index("      - name: Preserve bootstrap release notes")
    start = lines.index("        run: |", start) + 1
    end = start
    while end < len(lines) and (not lines[end].strip() or lines[end].startswith("          ")):
        end += 1
    return textwrap.dedent("\n".join(lines[start:end]))


SHELL = reconciliation_shell()

# This mock never contacts GitHub. Compare/head/commit evidence comes from the
# fixture's complete local repository, and every unimplemented request fails
# closed. __BRANCH__ is replaced with the component branch when installed.
GH_MOCK = '''#!/usr/bin/env python3
import base64, json, os, pathlib, subprocess, sys
state_path = pathlib.Path(os.environ["RETRY_STATE"])
state = json.loads(state_path.read_text())
args = sys.argv[1:]
assert args[0] == "api", args
repo = "repos/fixture/repo"
branch = "__BRANCH__"
NOTES_BRANCH = branch + "--release-notes"
git = os.environ["RETRY_REAL_GIT"]
remote = os.environ["RETRY_REMOTE"]
head = subprocess.check_output([git, "--git-dir", remote, "rev-parse", "refs/heads/" + branch], text=True).strip()
pr = {"number": 42, "state": "open", "base": {"ref": "main", "repo": {"full_name": "fixture/repo"}}, "head": {"ref": branch, "sha": head, "repo": {"full_name": state.get("head_repo", "fixture/repo")}}, "labels": [{"name": "autorelease: pending"}]}
if args[1].startswith(repo + "/pulls?"):
    assert "--paginate" in args and "--slurp" in args
    print(json.dumps([[pr] if state["open"] else []]))
elif args[1].startswith(repo + "/compare/"):
    main, compared_head = args[1].rsplit("/", 1)[1].split("...")
    assert compared_head == head
    base = subprocess.check_output([git, "merge-base", main, head], text=True).strip()
    print(json.dumps({"merge_base_commit": {"sha": base}, "status": "identical" if main == head else "ahead" if base == main else "diverged"}))
elif args[1].startswith(repo + "/pulls/42/commits"):
    assert "--paginate" in args and "--slurp" in args
    log = subprocess.check_output([git, "log", "--reverse", "--format=%H%x01%s", "refs/heads/" + branch], text=True).strip()
    entries = [{"sha": line.split("\\x01")[0], "commit": {"message": line.split("\\x01")[1]}} for line in log.splitlines()] if log else []
    print(json.dumps([entries]))
elif args[1].startswith(repo + "/commits/"):
    sha = args[1].rsplit("/", 1)[1]
    parents = subprocess.check_output([git, "show", "-s", "--format=%P", sha], text=True).strip()
    print(json.dumps({"parents": [{"sha": parent} for parent in parents.split()]}))
elif args == ["api", repo + "/pulls/42", "--jq", ".body"]:
    print(state["body"])
elif args[1] == repo + "/git/ref/heads/main" and args[3] == ".object.sha":
    print(state["main_sha"])
elif args[1] == repo + "/git/ref/heads/" + NOTES_BRANCH and args[3] == ".object.sha":
    if NOTES_BRANCH not in state["branches"]:
        sys.exit(1)
    print(state["branches"][NOTES_BRANCH])
elif args[:4] == ["api", "--method", "POST", repo + "/git/refs"]:
    # Native forkBranch (createFileOnNewBranch): the notes branch is created
    # from the default branch, and reused when a retried run finds it already
    # present. Both directions are enforced, not just scripted.
    state["branch_create_attempts"] += 1
    if state.get("fail_branch_create"):
        state["fail_branch_create"] = False
        state_path.write_text(json.dumps(state))
        sys.exit(1)
    pairs = [args[i + 1] for i, arg in enumerate(args) if arg == "-f" and i + 1 < len(args)]
    fields = dict(pair.split("=", 1) for pair in pairs)
    assert fields.get("ref") == "refs/heads/" + NOTES_BRANCH, fields
    assert fields.get("sha") == state["main_sha"], "Notes branch must be created from main"
    if NOTES_BRANCH in state["branches"]:
        sys.exit(1)
    state["branches"][NOTES_BRANCH] = fields["sha"]
    state["branch_creates"] += 1
    state_path.write_text(json.dumps(state))
elif "/contents/release-notes.md?ref=" in args[1]:
    if state.get("notes") is None:
        sys.exit(1)
    if "Accept: application/vnd.github.raw" in args:
        # Raw media carries the bytes at any size.
        sys.stdout.write(state["notes"])
    elif args[3] == ".content":
        # Documented Contents JSON boundary
        # (docs.github.com/rest/repos/contents): object bodies above 1 MiB
        # arrive as content "" / encoding "none". A JSON read therefore
        # resolves empty notes at that size and fails reconciliation loudly
        # before any push, PUT or dispatch, instead of publishing emptiness.
        if len(state["notes"].encode()) > 1024 * 1024:
            print("")
        else:
            print(base64.b64encode(state["notes"].encode()).decode())
    elif args[3] == ".sha":
        print(state["notes_sha"])
    else:
        raise AssertionError(args)
elif args[:4] == ["api", "--method", "PATCH", repo + "/pulls/42"]:
    state["patch_attempts"] += 1
    if state.get("fail_patch"):
        state["fail_patch"] = False
        state_path.write_text(json.dumps(state))
        sys.exit(1)
    payload = json.loads(pathlib.Path(args[args.index("--input") + 1]).read_text())
    state["body"] = payload["body"]
    state["patches"] += 1
    state_path.write_text(json.dumps(state))
elif args[1:4] == ["--method", "PUT", repo + "/contents/release-notes.md"]:
    state["notes_put_attempts"] += 1
    if state.get("fail_notes_put"):
        state["fail_notes_put"] = False
        state_path.write_text(json.dumps(state))
        sys.exit(1)
    # The Contents API cannot carry a file onto a missing branch: a PUT that
    # skipped branch creation fails here, before any mock state changes.
    assert NOTES_BRANCH in state["branches"], "Notes PUT without notes branch"
    payload = json.loads(pathlib.Path(args[args.index("--input") + 1]).read_text())
    # The Contents API reads the branch from the JSON payload: with --input,
    # gh puts -f field flags into the URL query instead (gh api --help), so a
    # payload without branch would target the default branch. Fail closed.
    assert payload.get("branch") == NOTES_BRANCH, payload.get("branch")
    if "sha" in payload:
        assert payload["sha"] == state["notes_sha"], (payload["sha"], state["notes_sha"])
    else:
        assert state["notes"] is None, "Create without sha requires absent file"
    state["notes"] = base64.b64decode(payload["content"]).decode()
    state["notes_sha"] = "notes-sha-%d" % state["notes_put_attempts"]
    state["notes_puts"] += 1
    state_path.write_text(json.dumps(state))
else:
    raise AssertionError(args)
'''

GIT_MOCK = '''#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
state_path = pathlib.Path(os.environ["RETRY_STATE"])
state = json.loads(state_path.read_text())
if sys.argv[1:3] == ["push", "origin"]:
    state["push_attempts"] += 1
    if state.get("fail_push"):
        state["fail_push"] = False
        state_path.write_text(json.dumps(state))
        sys.exit(1)
    result = subprocess.run([os.environ["RETRY_REAL_GIT"], *sys.argv[1:]])
    if result.returncode == 0:
        state["pushes"] += 1
    state_path.write_text(json.dumps(state))
    sys.exit(result.returncode)
sys.exit(subprocess.run([os.environ["RETRY_REAL_GIT"], *sys.argv[1:]]).returncode)
'''


class ReleaseRetryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP"))
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.repo = self.base / "repo"
        self.repo.mkdir()
        self.remote = self.base / "remote.git"
        self.real_git = shutil.which("git")
        self.git("init", "-b", "main")
        self.git("config", "user.name", "fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        scripts = self.repo / "scripts"
        scripts.mkdir()
        for name in ["migrate-release-notes.cjs", "release-pr-state.cjs"]:
            shutil.copyfile(ROOT / "scripts" / name, scripts / name)
        (self.repo / "CHANGELOG.md").write_text("# Changelog\n\n## Unreleased\n")
        self.git("add", ".")
        self.git("commit", "-m", "feat: fixture main")
        self.main = self.git("rev-parse", "HEAD").strip()
        # Only this disposable fixture switches branches; never the execution checkout.
        self.git("checkout", "-b", BRANCH)
        (self.repo / "CHANGELOG.md").write_text(CHANGELOG)
        self.git("add", "CHANGELOG.md")
        self.git("commit", "-m", "chore(main): release 0.2.0")
        self.native_commit = self.git("rev-parse", "HEAD").strip()
        self.git("init", "--bare", str(self.remote))
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "origin", "HEAD")
        self.state_path = self.base / "state.json"
        self.state_path.write_text(json.dumps({"open": True, "body": BODY, "patches": 0, "patch_attempts": 0, "pushes": 0, "push_attempts": 0, "notes": None, "notes_sha": "notes-sha-0", "notes_puts": 0, "notes_put_attempts": 0, "main_sha": self.main, "branches": {}, "branch_creates": 0, "branch_create_attempts": 0}))
        bin_path = self.base / "bin"
        bin_path.mkdir()
        mocks = [("gh", GH_MOCK.replace("__BRANCH__", BRANCH)), ("git", GIT_MOCK)]
        for name, source in mocks:
            file = bin_path / name
            file.write_text(source)
            file.chmod(0o755)
        self.output = self.base / "output"
        self.env = {
            "PATH": str(bin_path) + os.pathsep + os.environ["PATH"], "HOME": str(self.base),
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
            "RETRY_STATE": str(self.state_path), "RETRY_REAL_GIT": self.real_git,
            "RETRY_REMOTE": str(self.remote), "GH_REPO": "fixture/repo",
            "GITHUB_SHA": self.main, "GITHUB_OUTPUT": str(self.output),
            "RUNNER_TEMP": str(self.base), "PR_NUMBER": "42",
            "HEAD_BRANCH": BRANCH,
        }

    def git(self, *args, input=None):
        return subprocess.check_output([self.real_git, *args], cwd=self.repo, input=input, text=True, stderr=subprocess.DEVNULL)

    def state(self, **updates):
        state = json.loads(self.state_path.read_text())
        if updates:
            state.update(updates)
            # Native overflow bodies arrive with the notes branch already
            # created by native. Mirror that here: setting a stored-notes body
            # seeds the branch, so the native-overflow tests exercise reuse
            # while the grown-body tests exercise creation.
            if "body" in updates and updates["body"] == OVERFLOW_BODY and "branches" not in updates:
                state["branches"] = {NOTES_BRANCH: state["main_sha"]}
            self.state_path.write_text(json.dumps(state))
        return state

    def outputs(self, mode):
        self.output.write_text("")
        subprocess.run(["node", "scripts/release-pr-state.cjs", mode], cwd=self.repo, env=self.env, check=True, capture_output=True, text=True)
        return dict(line.split("=", 1) for line in self.output.read_text().splitlines())

    def reconcile(self, success=True):
        result = subprocess.run(["bash", "-euo", "pipefail", "-c", SHELL], cwd=self.repo, env=self.env, capture_output=True, text=True)
        if success:
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0)

    def fresh_checkout(self):
        # Recovery models a new runner, discarding ONLY this test's local work.
        self.git("fetch", "origin", BRANCH)
        self.git("reset", "--hard", "FETCH_HEAD")

    def assert_reconciled(self):
        notes = (self.repo / "CHANGELOG.md").read_text()
        self.assertEqual(notes.count("- historical RSVP repair"), 1)
        self.assertNotIn("## Unreleased", notes)
        self.assertEqual(self.state()["body"].count("- historical RSVP repair"), 1)
        self.assertEqual(self.git("status", "--porcelain"), "")

    def assert_overflow_reconciled(self):
        notes = (self.repo / "CHANGELOG.md").read_text()
        self.assertEqual(notes.count("- historical RSVP repair"), 1)
        self.assertNotIn("## Unreleased", notes)
        stored = self.state()["notes"]
        self.assertEqual(stored.count("- historical RSVP repair"), 1)
        self.assertEqual(self.state()["body"], OVERFLOW_BODY, "Overflow link is native-owned and never PATCHed")
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_unchanged_main_reuses_pr_without_disabling_publication(self):
        self.reconcile()
        before = self.state()
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "true"})
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertIn("skip-github-pull-request: ${{ steps.plan.outputs.reuse_pr == 'true' }}", workflow)
        self.assertNotIn("skip-github-release:", workflow)
        self.fresh_checkout()
        self.reconcile()
        self.assertEqual(self.state(), before, "No push or PATCH on unchanged rerun")
        self.assert_reconciled()

    def test_no_native_outputs_still_selects_existing_pr(self):
        self.assertEqual(self.outputs("select"), {"pr_available": "true", "pr": json.dumps({"number": 42, "headBranchName": BRANCH}, separators=(",", ":"))})
        self.reconcile()
        self.assert_reconciled()

    def test_pre_patch_migration_failure_recovers(self):
        self.state(body="unexpected release body")
        self.reconcile(False)
        self.assertEqual(self.state()["pushes"], 0)
        self.assertEqual(self.state()["patches"], 0)
        self.state(body=BODY)
        self.fresh_checkout()
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "true"})
        self.assertEqual(self.outputs("select")["pr_available"], "true")
        self.reconcile()
        self.assert_reconciled()

    def test_failed_patch_after_successful_push_recovers_body_only(self):
        self.state(fail_patch=True)
        self.reconcile(False)
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["patches"], 0)
        self.fresh_checkout()
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "true"})
        self.reconcile()
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["patch_attempts"], 2)
        self.assert_reconciled()

    def test_failed_push_recovers_without_patching_body_early(self):
        self.state(fail_push=True)
        self.reconcile(False)
        self.assertEqual(self.state()["body"], BODY)
        self.fresh_checkout()
        self.assertEqual(self.outputs("select")["pr_available"], "true")
        self.reconcile()
        self.assertEqual(self.state()["push_attempts"], 2)
        self.assertEqual(self.state()["patches"], 1)
        self.assert_reconciled()

    def test_already_migrated_body_repairs_changelog_only(self):
        migrated_body = BODY.replace("\n\n---\nRefs:", "\n\n### Fixed\n\n- historical RSVP repair\n\n---\nRefs:")
        self.state(body=migrated_body)
        self.reconcile()
        self.assertEqual(self.state()["patches"], 0)
        self.assertEqual(self.state()["pushes"], 1)
        self.assert_reconciled()

    def test_new_main_snapshot_regenerates_native_pr(self):
        new_main = self.git("commit-tree", "HEAD^{tree}", "-p", self.main, input="feat: next main snapshot\n").strip()
        self.env["GITHUB_SHA"] = new_main
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "false"})

    def test_update_branch_merge_regenerates_stale_release(self):
        # "Update branch" merges a newer main into the release branch: ancestry
        # then holds while the generated metadata is stale (native would bump
        # 0.1.1 to 0.2.0 for the new feature). Reuse must be snapshot-bound.
        self.git("checkout", "main")
        (self.repo / "next-feature.txt").write_text("next feature\n")
        self.git("add", "next-feature.txt")
        self.git("commit", "-m", "feat: next main snapshot")
        new_main = self.git("rev-parse", "HEAD").strip()
        self.git("checkout", BRANCH)
        self.git("merge", "--no-edit", new_main)
        self.git("push", "origin", BRANCH)
        head = self.git("rev-parse", "HEAD").strip()
        self.assertEqual(self.git("merge-base", new_main, head).strip(), new_main, "Ancestry holds after Update branch")
        self.env["GITHUB_SHA"] = new_main
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "false"})

    def test_no_open_pr_keeps_native_creation_and_publication_enabled(self):
        self.state(open=False)
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "false"})
        self.assertEqual(self.outputs("select"), {"pr_available": "false", "pr": ""})

    def test_foreign_head_is_not_selected(self):
        self.state(head_repo="foreign/repo")
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "false"})
        self.assertEqual(self.outputs("select")["pr_available"], "false")

    def test_overflow_reconciles_stored_notes_without_touching_link(self):
        self.state(body=OVERFLOW_BODY, notes=BODY)
        self.reconcile()
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["patches"], 0)
        self.assertEqual(self.state()["notes_puts"], 1)
        self.assert_overflow_reconciled()

    def test_overflow_failed_notes_put_recovers_notes_only(self):
        self.state(body=OVERFLOW_BODY, notes=BODY, fail_notes_put=True)
        self.reconcile(False)
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["notes_puts"], 0)
        self.fresh_checkout()
        self.reconcile()
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["notes_put_attempts"], 2)
        self.assertEqual(self.state()["patches"], 0)
        self.assert_overflow_reconciled()

    def test_overflow_dangling_link_fails_closed(self):
        self.state(body=OVERFLOW_BODY, notes=None)
        self.reconcile(False)
        self.assertEqual(self.state()["pushes"], 0)
        self.assertEqual(self.state()["patches"], 0)
        self.assertEqual(self.state()["notes_puts"], 0)

    def test_stale_notes_branch_alongside_normal_body_is_ignored(self):
        self.state(body=BODY, notes="stale stored notes")
        self.reconcile()
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["patches"], 1)
        self.assertEqual(self.state()["notes"], "stale stored notes")
        self.assertEqual(self.state()["notes_puts"], 0)
        self.assert_reconciled()

    def test_migration_grown_body_takes_overflow_path(self):
        # A normal native body whose migrated notes exceed the PR-body limit
        # (bulk lives in the first changelog section, as in the reviewer's
        # 351-commit native body) must take the overflow representation
        # instead of a PATCH the API would reject (and every retry repeat).
        filler = "\n".join(f"* generated item {i:04d} {'x' * 60}" for i in range(900))
        big_changelog = f"# Changelog\n\n{NOTES}\n\n{filler}\n\n## Changelog\n\n## Unreleased\n\n### Fixed\n\n- historical RSVP repair\n"
        big_body = f":robot: release\n---\n\n{NOTES}\n\n{filler}\n\n---\nRefs: TOG-9865\n"
        self.assertGreater(len(big_body), 65536, "Fixture must exceed the native body limit")
        (self.repo / "CHANGELOG.md").write_text(big_changelog)
        self.state(body=big_body)
        self.reconcile()
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["notes_puts"], 1)
        # The migration-created notes branch did not exist: the workflow must
        # have created it from main before the PUT, and the mock enforces both
        # directions (PUT without the branch fails closed).
        self.assertEqual(self.state()["branch_creates"], 1)
        self.assertEqual(self.state()["branches"], {NOTES_BRANCH: self.main})
        # The visible body really changes (normal body -> overflow link), so
        # exactly one small-link PATCH is published; the oversized text never
        # goes through PATCH.
        self.assertEqual(self.state()["patches"], 1)
        self.assertLess(len(self.state()["body"]), 1000)
        self.assertGreater(len(self.state()["notes"]), 65536)
        self.assertEqual(self.state()["notes"].count("- historical RSVP repair"), 1)
        overflow = self.state()["body"]
        self.assertNotIn("\n", overflow.strip())
        self.assertTrue(overflow.strip().startswith(OVERFLOW_SENTENCE))
        self.assertIn(NOTES_BRANCH, overflow)
        # Retry reconciles the new overflow representation without repeats.
        before = self.state()
        self.fresh_checkout()
        self.reconcile()
        overflow_state = self.state()
        self.assertEqual(overflow_state["pushes"], before["pushes"])
        self.assertEqual(overflow_state["notes_puts"], before["notes_puts"])
        self.assertEqual(overflow_state["branch_creates"], before["branch_creates"])
        self.assertEqual(overflow_state["patches"], before["patches"])

    def test_notes_put_without_branch_fails_before_dispatch(self):
        # The reviewer's exact P2 against the strict mock: the old workflow PUT
        # the migration-created notes onto a branch it never created. Invoke
        # the mock's PUT exactly as the workflow would, with the notes branch
        # absent: it must fail closed, changing no mock state and dispatching
        # nothing. This is the enforcement behind the workflow's create/reuse
        # step: skip that step and the run fails here instead.
        import base64 as b64lib
        self.state(body=OVERFLOW_BODY, notes=BODY, branches={})
        payload = self.base / "notes-payload.json"
        payload.write_text(json.dumps({
            "message": "chore(release): preserve bootstrap release notes",
            "content": b64lib.b64encode(b"updated notes").decode(),
            "branch": NOTES_BRANCH, "sha": self.state()["notes_sha"],
        }))
        before = self.state()
        result = subprocess.run(
            [str(self.base / "bin" / "gh"), "api", "--method", "PUT",
             "repos/fixture/repo/contents/release-notes.md",
             "--input", str(payload), "--silent"],
            env=self.env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0, "PUT onto a missing notes branch must fail")
        self.assertEqual(self.state(), before, "Failed PUT changes no mock state")
        self.assertEqual(self.state()["notes_puts"], 0)
        self.assertEqual(self.state()["patches"], 0)

    def test_failed_branch_create_recovers_without_repeating_work(self):
        filler = "\n".join(f"* generated item {i:04d} {'x' * 60}" for i in range(900))
        big_changelog = f"# Changelog\n\n{NOTES}\n\n{filler}\n\n## Changelog\n\n## Unreleased\n\n### Fixed\n\n- historical RSVP repair\n"
        big_body = f":robot: release\n---\n\n{NOTES}\n\n{filler}\n\n---\nRefs: TOG-9865\n"
        (self.repo / "CHANGELOG.md").write_text(big_changelog)
        self.state(body=big_body, fail_branch_create=True)
        self.reconcile(False)
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["branch_creates"], 0)
        self.assertEqual(self.state()["notes_puts"], 0)
        self.assertEqual(self.state()["patches"], 0)
        self.fresh_checkout()
        self.reconcile()
        self.assertEqual(self.state()["branch_create_attempts"], 2)
        self.assertEqual(self.state()["branch_creates"], 1)
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["notes_puts"], 1)
        self.assertEqual(self.state()["patches"], 1)

    def test_native_overflow_reuses_existing_notes_branch(self):
        # Native-created overflows arrive with the notes branch already in
        # place: no create call, just the notes update; a retry changes nothing.
        self.state(body=OVERFLOW_BODY, notes=BODY)
        self.reconcile()
        self.assertEqual(self.state()["branch_creates"], 0)
        self.assertEqual(self.state()["notes_puts"], 1)
        before = self.state()
        self.fresh_checkout()
        self.reconcile()
        self.assertEqual(self.state(), before, "No push, PUT, create or PATCH on unchanged overflow rerun")

    def test_overflow_above_contents_json_limit_reconciles_via_raw(self):
        # Documented Contents JSON boundary
        # (docs.github.com/rest/repos/contents): object bodies above 1 MiB
        # arrive as content "" / encoding "none". A 1,215,083-byte fixture
        # proves reconciliation reads the raw representation: the full stored
        # notes (with one RSVP repair) reconcile while the visible link stays
        # untouched. Reverting to a --jq .content read fails this test before
        # any push, PUT or dispatch.
        big_notes = BODY + "x" * (1215083 - len(BODY))
        self.assertGreater(len(big_notes.encode()), 1024 * 1024)
        self.state(body=OVERFLOW_BODY, notes=big_notes)
        self.reconcile()
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["patches"], 0)
        self.assertEqual(self.state()["notes_puts"], 1)
        self.assertEqual(self.state()["notes"].count("- historical RSVP repair"), 1)
        self.assertEqual(self.state()["body"], OVERFLOW_BODY)

    def test_normal_body_stays_on_patch_path(self):
        self.assertLess(len(BODY), 65536)
        self.reconcile()
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["patches"], 1)
        self.assertEqual(self.state()["notes_puts"], 0)
        self.assert_reconciled()


if __name__ == "__main__":
    unittest.main()
