"""Run the actual reconciliation shell with local Git and a fail-closed gh mock."""

import json
import os
from pathlib import Path
import re
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
pr = {"number": 42, "state": "open", "body": state["body"], "base": {"ref": "main", "repo": {"full_name": "fixture/repo"}}, "head": {"ref": branch, "sha": head, "repo": {"full_name": state.get("head_repo", "fixture/repo")}}, "labels": [{"name": "autorelease: pending"}]}
def emit_pages(entries):
    # Model the older gh CLI: @json emits one compact page per line, and
    # --slurp is unsupported. Never silently accept the incompatible flags.
    assert args[2:] == ["--paginate", "--jq", "@json"], args
    size = state.get("page_size", 1000)
    pages = [entries[i:i + size] for i in range(0, len(entries), size)] or [[]]
    if state.get("empty_first_page"):
        pages.insert(0, [])
    for page in pages:
        print(json.dumps(page, separators=(",", ":")))
if args[1].startswith(repo + "/pulls?"):
    emit_pages([pr] if state["open"] else [])
elif args[1].startswith(repo + "/compare/"):
    main, compared_head = args[1].rsplit("/", 1)[1].split("...")
    assert compared_head == head
    base = subprocess.check_output([git, "merge-base", main, head], text=True).strip()
    print(json.dumps({"merge_base_commit": {"sha": base}, "status": "identical" if main == head else "ahead" if base == main else "diverged"}))
elif args[1].startswith(repo + "/pulls/42/commits"):
    log = subprocess.check_output([git, "log", "--reverse", "--format=%H%x01%s", "refs/heads/" + branch], text=True).strip()
    entries = [{"sha": line.split("\\x01")[0], "commit": {"message": line.split("\\x01")[1] + state.get("commit_message_tail", "")}} for line in log.splitlines()] if log else []
    emit_pages(entries)
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


def workflow_text():
    return (ROOT / ".github/workflows/release.yml").read_text()


def job_steps(job_id):
    """Step blocks of one release.yml job, as raw text starting after the `- `."""
    match = re.search(rf"(?ms)^  {re.escape(job_id)}:\n(.*?)(?=^  [\w-]+:\n|\Z)", workflow_text())
    assert match, job_id
    return re.split(r"(?m)^      - ", match.group(1))[1:]


def step_with(job_id, marker):
    steps = [step for step in job_steps(job_id) if marker in step]
    assert len(steps) == 1, (job_id, marker, len(steps))
    return steps[0]


def step_condition(step):
    conditions = re.findall(r"(?m)^        if: (.+)$", step)
    assert len(conditions) <= 1, conditions
    return conditions[0] if conditions else None


def evaluate(expression, context):
    """Evaluate the `a == 'x' && b != 'y' || c == 'z'` conditions this workflow uses.

    Deliberately tiny and fail-closed: any other syntax raises instead of guessing.
    """
    def atom(text):
        match = re.fullmatch(r"\s*([\w.\-]+)\s*(==|!=)\s*'([^']*)'\s*", text)
        assert match, f"Unsupported expression: {text!r}"
        name, operator, literal = match.groups()
        equal = context[name] == literal
        return equal if operator == "==" else not equal
    return any(all(atom(part) for part in clause.split("&&")) for clause in expression.split("||"))


class ReleaseTriggerShapeTests(unittest.TestCase):
    """TOG-12931: push publishes only; schedule/dispatch regenerate and dispatch checks."""

    def context(self, event, pr_available="true", reuse_pr="", parse_guard="success"):
        return {
            "github.event_name": event,
            "steps.parse_guard.outcome": parse_guard,
            "steps.select.outputs.pr_available": pr_available,
            "needs.release-please.outputs.pr_available": pr_available,
            "steps.plan.outputs.reuse_pr": reuse_pr,
        }

    def skip_expression(self):
        step = step_with("release-please", "googleapis/release-please-action@")
        match = re.search(r"(?m)^          skip-github-pull-request: \$\{\{ (.+) \}\}$", step)
        self.assertIsNotNone(match)
        return match.group(1)

    def conditions(self):
        return {
            "plan": step_condition(step_with("release-please", "id: plan")),
            "checkout": step_condition(step_with("release-please", "ref: ${{ fromJSON(steps.select.outputs.pr).headBranchName }}")),
            "preserve": step_condition(step_with("release-please", "name: Preserve bootstrap release notes")),
            "dispatch": re.search(r"(?m)^    if: (.*needs\.release-please\.outputs\.pr_available.*)$", workflow_text()).group(1),
        }

    def test_triggers_are_push_schedule_and_dispatch_only(self):
        on = workflow_text().split("\npermissions:", 1)[0]
        self.assertIn("\n  push:\n    branches: [main]\n", on)
        self.assertRegex(on, r"(?m)^  schedule:\n(?:    #[^\n]*\n)*    - cron: '\d+ \d+ \* \* [\d*]'$")
        self.assertEqual(on.count("- cron:"), 1)
        self.assertIn("\n  workflow_dispatch:\n", on)
        self.assertNotIn("pull_request", on)

    def test_push_publishes_without_regenerating_or_dispatching(self):
        skip, conditions = self.skip_expression(), self.conditions()
        self.assertNotIn("skip-github-release:", workflow_text())
        for pr_available in ("true", "false"):
            for reuse_pr in ("", "true", "false"):
                with self.subTest(pr_available=pr_available, reuse_pr=reuse_pr):
                    context = self.context("push", pr_available, reuse_pr)
                    self.assertTrue(evaluate(skip, context), "PR generation is skipped on push; publication is not")
                    for name, condition in conditions.items():
                        self.assertFalse(evaluate(condition, context), f"{name} must not run on push")

    def test_schedule_and_dispatch_regenerate_reconcile_and_dispatch(self):
        skip, conditions = self.skip_expression(), self.conditions()
        for event in ("schedule", "workflow_dispatch"):
            for reuse_pr in ("true", "false"):
                with self.subTest(event=event, reuse_pr=reuse_pr):
                    self.assertTrue(evaluate(conditions["plan"], self.context(event, "true", reuse_pr)))
                    self.assertEqual(evaluate(skip, self.context(event, "true", reuse_pr)), reuse_pr == "true")
            for pr_available in ("true", "false"):
                with self.subTest(event=event, pr_available=pr_available):
                    context = self.context(event, pr_available, "false")
                    for name in ("checkout", "preserve", "dispatch"):
                        self.assertEqual(evaluate(conditions[name], context), pr_available == "true", name)

    def test_failed_parse_guard_skips_regeneration_but_not_publication(self):
        skip, conditions = self.skip_expression(), self.conditions()
        for event in ("schedule", "workflow_dispatch"):
            for reuse_pr in ("", "true", "false"):
                with self.subTest(event=event, reuse_pr=reuse_pr):
                    context = self.context(event, "true", reuse_pr, parse_guard="failure")
                    self.assertTrue(evaluate(skip, context), "a failed guard must not rebuild the release PR")
                    for name in ("checkout", "preserve"):
                        self.assertFalse(evaluate(conditions[name], context), f"{name} follows a regeneration only")
        # A guard that was skipped (push) or passed leaves the regeneration rules unchanged.
        for outcome in ("success", "skipped"):
            context = self.context("schedule", "true", "false", parse_guard=outcome)
            self.assertFalse(evaluate(skip, context))
            self.assertTrue(evaluate(conditions["checkout"], context))
        text = workflow_text()
        self.assertNotIn("skip-github-release:", text, "publication stays enabled when the guard fails")
        guard = step_with("release-please", "id: parse_guard")
        self.assertIn("continue-on-error: true", guard, "the guard must not stop the publishing action")
        self.assertLess(text.index("id: parse_guard"), text.index("googleapis/release-please-action@"))

    def test_failed_parse_guard_fails_the_job_after_publication(self):
        steps = job_steps("release-please")
        final = steps[-1]
        self.assertIn("name: Fail the run when release-please could not parse every commit", final)
        self.assertEqual(step_condition(final), "${{ !cancelled() && steps.parse_guard.outcome == 'failure' }}")
        self.assertIn("exit 1", final)
        action = next(index for index, step in enumerate(steps) if "googleapis/release-please-action@" in step)
        self.assertGreater(len(steps) - 1, action, "the failing step comes after the publishing action")
        # dispatch-checks keeps the implicit success() gate, so the failed job never dispatches checks;
        # sbom-target runs under !cancelled() and still reads release_created.
        dispatch = re.search(r"(?ms)^  dispatch-checks:\n(.*?)(?=^  [\w-]+:\n)", workflow_text()).group(1)
        self.assertNotRegex(dispatch, r"(?m)^    if: .*(always|failure|cancelled)\(")
        sbom = re.search(r"(?ms)^  sbom-target:\n(.*?)(?=^  [\w-]+:\n)", workflow_text()).group(1)
        self.assertIn("!cancelled()", sbom)
        self.assertIn("needs.release-please.outputs.release_created == 'true'", sbom)

    def test_publication_path_and_permissions_are_untouched(self):
        workflow = workflow_text()
        self.assertIn("    if: github.ref == 'refs/heads/main' && inputs.dry_run != true && (inputs.release_tag == '' || github.event_name != 'workflow_dispatch')\n", workflow)
        self.assertIn("      contents: write # Publish releases and reconcile the release/notes branches.\n      pull-requests: write # Create and reconcile the release PR.\n", workflow)
        self.assertIn("      actions: write # Dispatch required checks for GITHUB_TOKEN-created release PRs.\n", workflow)
        self.assertIn("needs.release-please.outputs.release_created == 'true'", workflow)
        self.assertIn("gh workflow run check.yml --ref \"$HEAD_BRANCH\"", workflow)
        self.assertIn("gh workflow run supply-chain.yml --ref \"$HEAD_BRANCH\" -f pr_number=\"$PR_NUMBER\"", workflow)


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
        self.assertIn("skip-github-pull-request: ${{ github.event_name == 'push' || steps.parse_guard.outcome == 'failure' || steps.plan.outputs.reuse_pr == 'true' }}", workflow)
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

    def test_post_release_unreleased_prefix_recovers_failed_patch(self):
        current = "## [0.3.0](https://github.com/fixture/repo/compare/v0.2.0...v0.3.0)\n\n### Added\n\n* generated sticky feature"
        pending = "### Added\n\n- pending sticky runtime\n\n### Notes\n\n- pending runtime caveat"
        history = "## 0.2.0\n\n### Fixed\n\n- published bootstrap repair\n\n### Notes\n\n- published caveat\n"
        changelog = f"# Changelog\n\n## Unreleased\n\n{pending}\n\n{current}\n\n{history}"
        body = f":robot: release\n---\n\n{current}\n\n---\nRefs: TOG-9865\n"
        (self.repo / "CHANGELOG.md").write_text(changelog)
        self.git("add", "CHANGELOG.md")
        self.git("commit", "-m", "chore(main): release 0.3.0")
        self.git("push", "origin", "HEAD")
        self.state(body=body, fail_patch=True)
        self.reconcile(False)
        self.assertEqual(self.state()["pushes"], 1)
        self.assertEqual(self.state()["patches"], 0)
        self.fresh_checkout()
        self.reconcile()
        notes = (self.repo / "CHANGELOG.md").read_text()
        migrated_body = self.state()["body"]
        self.assertTrue(notes.endswith(history), "Published history stays byte-for-byte intact")
        self.assertNotIn("## Unreleased", notes)
        for note in ["- pending sticky runtime", "- pending runtime caveat"]:
            self.assertEqual(notes.count(note), 1)
            self.assertEqual(migrated_body.count(note), 1)
        self.assertNotIn("- published bootstrap repair", migrated_body)
        self.assertNotIn("- published caveat", migrated_body)
        self.assertTrue(migrated_body.startswith(":robot: release\n---\n\n"))
        self.assertTrue(migrated_body.endswith("\n\n---\nRefs: TOG-9865\n\n"))
        self.assertEqual(self.state()["pushes"], 1, "Retry must not push the changelog twice")
        self.assertEqual(self.state()["patch_attempts"], 2)
        self.assertEqual(self.state()["patches"], 1)
        before = self.state()
        self.fresh_checkout()
        self.reconcile()
        self.assertEqual(self.state(), before, "Completed prefix reconciliation is a no-op")
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_fenced_unreleased_notes_fail_before_any_write(self):
        current = "## 0.3.0\n\n### Added\n\n* generated feature"
        history = "## 0.2.0\n\n### Fixed\n\n- published repair\n"
        for fence in ["```markdown", "~~~markdown", "   ````markdown"]:
            with self.subTest(fence=fence):
                # Exact layout emitted by native 17.6.0: it inserts the release
                # inside the pending fence before the version-shaped example.
                changelog = f"# Changelog\n\n## Unreleased\n\n### Notes\n\n- Pending example:\n\n{fence}\n{current}\n\n## 1.2.3\n{fence}\n\n- pending caveat AFTER example\n\n{history}"
                body = f":robot: release\n---\n\n{current}\n\n---\nRefs: TOG-9865\n"
                (self.repo / "CHANGELOG.md").write_text(changelog)
                self.git("add", "CHANGELOG.md")
                self.git("commit", "-m", "chore(main): release 0.3.0")
                self.git("push", "origin", "HEAD")
                self.state(body=body)
                before = self.state()
                head = self.git("rev-parse", "HEAD")
                for _ in range(2):
                    self.reconcile(False)
                    self.assertEqual((self.repo / "CHANGELOG.md").read_text(), changelog)
                    self.assertEqual(self.state(), before, "No push, PATCH, PUT or branch creation before rejection, including retries")
                    self.assertEqual(self.git("rev-parse", "HEAD"), head)
                    self.assertEqual(self.git("status", "--porcelain"), "")

    def test_old_gh_pagination_reads_later_pages_and_preserves_notes(self):
        footer = '\nQuoted "notes" and a backslash \\ remain intact.\n'
        self.state(page_size=1, empty_first_page=True, body=BODY + footer,
                   commit_message_tail='\n\nMultiline "message" with \\ and Unicode ✓')
        before = self.state()
        # The selected PR and newest native commit are both on later pages.
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "true"})
        outputs = self.outputs("select")
        self.assertEqual(outputs["pr_available"], "true")
        self.assertEqual(json.loads(outputs["pr"]), {"number": 42, "headBranchName": BRANCH})
        self.assertEqual(self.state(), before, "Inspection is read-only")
        self.reconcile()
        self.assert_reconciled()
        self.assertIn(footer, self.state()["body"])

    def test_new_main_snapshot_regenerates_native_pr(self):
        new_main = self.git("commit-tree", "HEAD^{tree}", "-p", self.main, input="feat: next main snapshot\n").strip()
        self.env["GITHUB_SHA"] = new_main
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "false"})

    def test_main_running_ahead_of_untouched_pr_is_stale_until_regenerated(self):
        # A push no longer regenerates the PR (TOG-12931), so main routinely
        # runs several commits ahead of it. The dispatch/schedule plan - and the
        # docs/releases.md freshness check - must call that stale, and fresh
        # again only once native force-replaces the branch on the new main.
        self.git("checkout", "main")
        for index in range(3):
            (self.repo / f"main-{index}.txt").write_text("main\n")
            self.git("add", f"main-{index}.txt")
            self.git("commit", "-m", f"fix: main {index}")
        new_main = self.git("rev-parse", "HEAD").strip()
        self.env["GITHUB_SHA"] = new_main
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "false"})
        self.git("checkout", "-B", BRANCH, new_main)
        (self.repo / "CHANGELOG.md").write_text(CHANGELOG)
        self.git("add", "CHANGELOG.md")
        self.git("commit", "-m", "chore(main): release 0.2.0")
        self.git("push", "--force", "origin", BRANCH)
        self.assertEqual(self.outputs("plan"), {"reuse_pr": "true"})

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
        before = self.state()
        outputs = self.outputs("select")
        self.assertEqual(outputs, {"pr_available": "false", "pr": "{}"})
        # The live first-release run published successfully, then failed while
        # evaluating fromJSON('') in the skipped reconciliation step's env.
        # Exercise the actual CLI output: it must parse even before if is
        # applied, and neither selected-PR field may identify a mutation target.
        selected = json.loads(outputs["pr"])
        self.assertIsNone(selected.get("number"))
        self.assertIsNone(selected.get("headBranchName"))
        with self.assertRaises(json.JSONDecodeError):
            json.loads("")  # Negative control: original post-publication value.
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        self.assertEqual(workflow.count("if: github.event_name != 'push' && steps.parse_guard.outcome != 'failure' && steps.select.outputs.pr_available == 'true'"), 2)
        self.assertIn("if: github.event_name != 'push' && needs.release-please.outputs.pr_available == 'true'", workflow)
        self.assertNotIn("skip-github-release:", workflow)
        self.assertEqual(self.state(), before, "No push, PATCH, notes PUT or branch creation")

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
