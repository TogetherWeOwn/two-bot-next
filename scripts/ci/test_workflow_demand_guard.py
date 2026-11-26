"""Regression fixtures for workflow demand and the required pr-lint entry."""

import importlib.util
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"
SPEC = importlib.util.spec_from_file_location("workflow_guard", ROOT / "ci/workflow_guard.py")
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)


def workflow(name):
    return guard.yaml.load((WORKFLOWS / name).read_text(encoding="utf-8"), Loader=guard.UniqueKeyLoader)


def example(triggers, header=""):
    return (f"{header}\nname: fixture\non: {triggers}\njobs:\n  ci-ok: {{runs-on: ubuntu-latest}}\n"
            .encode("utf-8"))


class WorkflowDemandGuardTests(unittest.TestCase):
    def test_current_workflows_pass_guard(self):
        self.assertEqual(guard.check_directory(WORKFLOWS), [])

    def test_automatic_triggers_need_reviewed_purpose(self):
        for trigger in ("pull_request", "push", "workflow_run", "merge_group", "issue_comment",
                        "pull_request_review", "pull_request_review_comment", "check_run", "check_suite", "status"):
            with self.subTest(trigger=trigger):
                self.assertTrue(guard.findings("fixture.yml", example(trigger)))
        marker = "# guards: required:ci-ok"
        self.assertTrue(guard.findings("fixture.yml", example("pull_request", marker)))
        size = WORKFLOWS / "pr-size.yml"
        self.assertEqual(guard.findings(size.name, size.read_bytes()), [])
        self.assertTrue(guard.findings(size.name, size.read_bytes().replace(b"pr-size-gate / gate", b"pr-size-gate")))

    def test_paths_apply_to_every_automatic_trigger(self):
        marker = "# guards: paths (fixture)"
        scoped = "{pull_request: {paths: ['src/**']}, push: {branches: [work], paths: ['src/**']}}"
        self.assertEqual(guard.findings("fixture.yml", example(scoped, marker).replace(b"\n", b"\r\n")), [])
        unscoped = "{pull_request: {paths: ['src/**']}, push: {branches: [work]}}"
        self.assertIn("push declares no paths filter", guard.findings("fixture.yml", example(unscoped, marker)))
        for trigger in ("{pull_request: {paths: ['**/**']}}", "{pull_request: {paths: ['*', '*/**']}}",
                        "{pull_request: {paths-ignore: [missing]}}", "{push: {paths: [src]}}",
                        "{push: {branches: [work], tags: ['**'], paths: [src]}}"):
            self.assertTrue(guard.findings("fixture.yml", example(trigger, marker)), trigger)

    def test_refuses_duplicate_pr_main_push_and_metadata_events(self):
        marker = "# guards: required:ci-ok"
        for branches in ("{}", "{branches: [main]}", "{branches: ['m*']}"):
            triggers = "{pull_request_target: {}, push: " + branches + "}"
            with self.subTest(branches=branches):
                self.assertIn("runs on PRs and main pushes without a reviewed release guard",
                              guard.findings("fixture.yml", example(triggers, marker)))
        for event in guard.PR_EVENTS:
            triggers = "{" + event + ": {types: [edited]}}"
            self.assertIn(f"{event} types include 'edited'", guard.findings("fixture.yml", example(triggers, marker)))
        supply = WORKFLOWS / "supply-chain.yml"
        self.assertEqual(guard.findings(supply.name, supply.read_bytes()), [])

    def test_release_notifications_are_completed_only(self):
        marker = "# guards: release:production-promotion"
        for triggers in ("{workflow_run: {}}", "{workflow_run: {types: [requested, in_progress, completed]}}"):
            with self.subTest(triggers=triggers):
                self.assertIn("workflow_run must fire only on completed, never requested/in_progress",
                              guard.findings("deploy-production.yml", example(triggers, marker)))
        source = (WORKFLOWS / "deploy-production.yml").read_bytes()
        self.assertEqual(guard.findings("deploy-production.yml", source), [])
        self.assertTrue(guard.findings("deploy-production.yml", source.replace(b"[deploy-staging, check]", b"[check]")))

    def test_malformed_files_fail_without_printing_their_content(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            self.assertEqual(guard.check_directory(directory), ["No workflows found"])
            (directory / "duplicate.yml").write_bytes(
                example("{pull_request: {}, pull_request: {}}", "# synthetic-value"))
            (directory / "link.yml").symlink_to(directory / "duplicate.yml")
            (directory / "bad-types.yml").write_bytes(example("{pull_request: {types: edited}}"))
            (directory / "merge-key.yml").write_bytes(example("{<<: {pull_request: {}}}"))
            self.assertEqual(guard.check_directory(directory), [
                f"{name}.yml: unreadable YAML or invalid workflow trigger configuration"
                for name in ("bad-types", "duplicate", "link", "merge-key")
            ])

    def test_real_advisory_and_protected_event_routes(self):
        nightly = workflow("nightly.yml")
        self.assertEqual(set(nightly["on"]), {"schedule", "workflow_dispatch"})
        self.assertEqual(nightly["jobs"]["pipeline-benchmark"]["uses"], "./.github/workflows/pipeline-benchmark.yml")
        self.assertIn("Cargo.lock", workflow("pipeline-benchmark.yml")["on"]["pull_request"]["paths"])
        self.assertEqual(set(workflow("ci-speed.yml")["on"]), {"workflow_dispatch"})
        evidence = workflow("pr-evidence.yml")["on"]
        self.assertEqual(evidence["pull_request"]["types"],
                         ["opened", "edited", "synchronize", "reopened", "ready_for_review"])
        self.assertEqual(evidence["push"]["paths"], [".github/scripts/*pr*evidence*", ".github/workflows/pr-evidence.yml"])
        supply = workflow("supply-chain.yml")
        self.assertEqual(set(supply["on"]["pull_request"]["types"]) - {"ready_for_review"},
                         {"opened", "edited", "synchronize", "reopened"})
        self.assertEqual(supply["on"]["push"], {"branches": ["main"]})
        self.assertEqual(supply["jobs"]["gitleaks"]["needs"], "pr-lint")
        for name in ("release.yml", "deploy-staging.yml", "deploy-production.yml", "staging-rollback-drill.yml"):
            self.assertIn("workflow_dispatch", workflow(name)["on"])

    def test_required_pr_lint_runs_trusted_guard_before_candidate_code(self):
        job = workflow("supply-chain.yml")["jobs"]["pr-lint"]
        self.assertEqual(job["name"], "pr-lint")
        steps = {step.get("name"): step for step in job["steps"]}
        name = "Guard automatic workflow demand"
        names = list(steps)
        self.assertLess(names.index("Verify base-branch gate snapshot"), names.index(name))
        self.assertLess(names.index(name), names.index("Unit-test the lint"))
        step = steps[name]
        self.assertNotIn("if", step)
        self.assertNotIn("continue-on-error", step)
        for fragment in ('"$GITHUB_EVENT_NAME" != push', 'policy="$policy/base-gate"',
                         '-I "$policy/ci/workflow_guard.py" "$GITHUB_WORKSPACE/.github/workflows"'):
            self.assertIn(fragment, step["run"])
