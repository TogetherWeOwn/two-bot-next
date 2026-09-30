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
elif "/contents/release-notes.md?ref=" in args[1]:
    if state.get("notes") is None:
        sys.exit(1)
    if args[3] == ".content":
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
    payload = json.loads(pathlib.Path(args[args.index("--input") + 1]).read_text())
    assert payload["sha"] == state["notes_sha"], (payload["sha"], state["notes_sha"])
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
        self.state_path.write_text(json.dumps({"open": True, "body": BODY, "patches": 0, "patch_attempts": 0, "pushes": 0, "push_attempts": 0, "notes": None, "notes_sha": "notes-sha-0", "notes_puts": 0, "notes_put_attempts": 0}))
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


if __name__ == "__main__":
    unittest.main()
