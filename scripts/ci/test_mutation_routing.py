"""Offline admission and changed-line proof for the advisory mutation lanes."""

import json
import os
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/mutation.yml"
SCOPE = runpy.run_path(str(ROOT / ".github/scripts/mutation_scope.py"))
ROUTES = {"rust": ("rust", "fixture"), "worker": ("worker", "worker_fixture")}
PYTHON_JOB = "python-mutation"


class MutationRoutingTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.repo = scratch / "repo"
        self.repo.mkdir()
        self.output = scratch / "output"
        self.jobs = yaml.load(WORKFLOW.read_text(), Loader=yaml.BaseLoader)["jobs"]
        self.git("init", "-q")
        self.write("src/old.rs", "\n".join(f"// line {i}" for i in range(1, 11)) + "\n")
        self.write("scripts/removed.py", "REMOVED = True\n")
        self.write("wrangler/src/example.ts", "const old = true;\n")
        self.write(".github/scripts/mutation_rust_fixture.py", "# original fixture\n")
        self.common = self.commit("common")
        self.write("src/base-only.rs", "// not in the PR\n")
        self.write("wrangler/src/example.ts", "const baseOnly = true;\n")
        self.base = self.commit("unrelated base updates")
        self.git("checkout", "-q", "--detach", self.common)

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", *args],
            cwd=self.repo, text=True, timeout=10,
        ).strip()

    def write(self, name, content):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def commit(self, message):
        self.git("add", "--all")
        self.git("commit", "-qm", message)
        return self.git("rev-parse", "HEAD")

    def plan(self, head, base=None):
        self.output.write_text("")
        with patch.dict(os.environ, BASE_SHA=base or self.base, HEAD_SHA=head,
                        RUNNER_TEMP=str(self.output.parent), GITHUB_OUTPUT=str(self.output)):
            SCOPE["main"](self.repo)
        outputs = dict(line.split("=") for line in self.output.read_text().splitlines())
        diff = self.output.with_name("mutation.diff").read_text()
        worker = json.loads(self.output.with_name("worker-mutation-plan.json").read_text())
        return outputs, SCOPE["added_lines"](diff), worker

    def admission(self, outputs, result="success", cancelled=False):
        # Pin the Actions conditions before replaying their string comparisons.
        self.assertEqual(set(self.jobs), {PYTHON_JOB, "rust-mutation", "worker-mutation"})
        self.assertNotIn("needs", self.jobs[PYTHON_JOB])
        self.assertNotIn("if", self.jobs[PYTHON_JOB])
        self.assertEqual(self.jobs[PYTHON_JOB].get("outputs"), {
            key: f"${{{{ steps.plan.outputs.{key} }}}}" for pair in ROUTES.values() for key in pair
        })
        admitted = set() if cancelled else {"python"}
        for lane, (measure, fixture) in ROUTES.items():
            job = self.jobs[f"{lane}-mutation"]
            self.assertEqual(job["needs"], PYTHON_JOB)
            self.assertEqual(job["if"],
                             f"${{{{ !cancelled() && (needs.{PYTHON_JOB}.result != 'success' ||\n"
                             f"    needs.{PYTHON_JOB}.outputs.{measure} != 'false' ||\n"
                             f"    needs.{PYTHON_JOB}.outputs.{fixture} != 'false') }}}}")
            if not cancelled and (result != "success" or any(
                    outputs.get(key, "").lower() != "false" for key in (measure, fixture))):
                admitted.add(lane)
        return admitted

    def assert_evidence(self, outputs, admitted, measured):
        receipts = set()
        for lane in admitted:
            job = self.jobs[f"{lane}-mutation"]
            checkout = next(step for step in job["steps"] if step.get("uses", "").startswith("actions/checkout@"))
            self.assertEqual(checkout["with"]["ref"], "${{ github.event.pull_request.head.sha }}")
            plan = next(step for step in job["steps"] if step.get("id") == "plan")
            self.assertEqual(plan["env"], {key + "_SHA": f"${{{{ github.event.pull_request.{key.lower()}.sha }}}}"
                                           for key in ("BASE", "HEAD")})
            fixture = next(step for step in job["steps"] if step.get("name", "").startswith("Prove "))
            fixture_output = "fixture" if lane == "rust" else f"{lane}_fixture"
            self.assertEqual(fixture["if"], f"steps.plan.outputs.{fixture_output} == 'true'")
            measurement = next(step for step in job["steps"] if step.get("name", "").startswith("Measure "))
            self.assertEqual(measurement["if"], f"steps.plan.outputs.{lane} == 'true'")
            upload = next(step for step in job["steps"] if step.get("uses", "").startswith("actions/upload-artifact@"))
            self.assertEqual(upload["if"], f"${{{{ always() && steps.plan.outputs.{lane} == 'true' }}}}")
            self.assertEqual(upload["with"]["name"], f"{lane}-mutation")
            self.assertEqual(upload["with"]["path"], f"${{{{ runner.temp }}}}/{lane}-mutation.json")
            self.assertEqual(upload["with"]["if-no-files-found"], "error")
            if outputs[lane] == "true":
                receipts.add(lane)
        self.assertEqual(receipts, measured)

    def test_exact_merge_base_scope_and_advisory_receipts(self):
        self.git("mv", "src/old.rs", "src/renamed file.rs")
        with (self.repo / "src/renamed file.rs").open("a") as target:
            target.write("// new line\n")
        self.git("rm", "-q", "scripts/removed.py")
        self.write("scripts/example.py", "VALUE = 1\n")
        self.write("wrangler/src/example.ts", "const old = true;\nconst added = false;\n")
        head = self.commit("rename with an addition, delete and edit")
        self.assertEqual(self.git("merge-base", self.base, head), self.common)
        changes = self.git("diff", "--find-renames", "--name-status", self.common, head).splitlines()
        self.assertIn("D\tscripts/removed.py", changes)
        self.assertTrue(any(row.startswith("R") and row.endswith("\tsrc/old.rs\tsrc/renamed file.rs")
                            for row in changes))
        outputs, added, worker = self.plan(head)
        self.assertEqual(added, {"src/renamed file.rs": {11}, "scripts/example.py": {1},
                                 "wrangler/src/example.ts": {2}})
        self.assertEqual(worker, {"wrangler/src/example.ts": [2]})
        admitted = self.admission(outputs)
        self.assertEqual(admitted, {"rust", "python", "worker"})
        self.assert_evidence(outputs, admitted, admitted)

    def test_no_work_and_fixture_only_admissions(self):
        self.git("mv", "src/old.rs", "src/moved.rs")
        self.git("rm", "-q", "scripts/removed.py")
        self.write("README.md", "# docs\n")
        outputs, added, _ = self.plan(self.commit("pure rename, deletion and documentation"))
        self.assertEqual(added, {})
        self.assertEqual(self.admission(outputs), {"python"})
        self.assert_evidence(outputs, {"python"}, set())

        self.git("mv", ".github/scripts/mutation_rust_fixture.py", "retired-fixture.txt")
        outputs, added, _ = self.plan(self.commit("move a Rust fixture away"))
        self.assertEqual(added, {})
        self.assertEqual(outputs["python_fixture"], "true")
        self.assertEqual(self.admission(outputs), {"python", "rust"})
        self.assert_evidence(outputs, {"python", "rust"}, set())

        self.write(".gitattributes", "*.md text\n")
        self.write(".github/workflows/mutation.yml", "# routing update\n")
        outputs, added, _ = self.plan(self.commit("shared mutation configuration"))
        self.assertEqual(added, {})
        self.assertEqual(self.admission(outputs), {"python", "rust", "worker"})
        self.assert_evidence(outputs, {"python", "rust", "worker"}, set())

    def test_failed_or_timed_out_planner_admits_lanes_to_recompute(self):
        self.write("wrangler/src/example.ts", "const old = true;\nconst added = false;\n")
        head = self.commit("Worker addition")
        with self.assertRaises(subprocess.CalledProcessError):
            self.plan(head, base="0" * 40)
        self.assertEqual(self.output.read_text(), "")
        with patch.dict(SCOPE["pr_diff"].__globals__, git=self.timed_out_git):
            with self.assertRaises(subprocess.TimeoutExpired):
                self.plan(head)
        self.assertEqual(self.output.read_text(), "")
        outputs, _, _ = self.plan(head)
        all_lanes = {"python", "rust", "worker"}
        for result, routing in (("failure", {}), ("cancelled", {}), ("success", {}),
                                ("failure", dict.fromkeys(outputs, "false"))):
            with self.subTest(result=result, routing=routing):
                self.assertEqual(self.admission(routing, result=result), all_lanes)
                self.assert_evidence(outputs, all_lanes, {"worker"})
        self.assertEqual(self.admission(outputs), {"python", "worker"})
        self.assertEqual(self.admission({}, result="cancelled", cancelled=True), set())

    @staticmethod
    def timed_out_git(root, *args):
        raise subprocess.TimeoutExpired(["git", *args], 30)


if __name__ == "__main__":
    unittest.main()
