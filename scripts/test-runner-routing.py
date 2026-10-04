"""Pin one runner-routing expression on every job of every workflow (TOG-12339).

The repo is public and the org's self-hosted runner group refuses public
repos, so a job with a bare `[self-hosted, two-selfhosted]` label queues
forever, and a required check behind it (or behind its `needs:`) never
reports. Every job must use the routing expression documented on `check` in
check.yml: public repo -> GitHub-hosted, private -> CI_OVERFLOW_* switch, then
self-hosted. Offline and stdlib-only: no GitHub credentials, no PyYAML.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github/workflows"
ROUTED = (
    "${{ fromJSON((!github.event.repository.private && '[\"ubuntu-latest\"]') || "
    "(contains(fromJSON(vars.CI_OVERFLOW_JOBS || '[]'), '{job}') && "
    "contains(fromJSON(vars.CI_OVERFLOW_EVENTS || '[]'), github.event_name) && "
    "vars.CI_OVERFLOW_RUNNER) || '[\"self-hosted\",\"two-selfhosted\"]') }}"
)


def routed(job):
    return ROUTED.replace("{job}", job)


def jobs(text):
    """Map each job id under the top-level `jobs:` key to its own lines."""
    found, job, inside = {}, None, False
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if not line.startswith(" "):
            inside, job = line.rstrip() == "jobs:", None
            continue
        if inside and re.match(r"^  [A-Za-z0-9_-]+:\s*$", line):
            job = line.strip()[:-1]
            found[job] = []
        elif inside and job is not None:
            found[job].append(line)
    return found


def violations(text):
    """Every way a workflow can leave a job on an unroutable runner label."""
    problems = []
    for job, lines in jobs(text).items():
        keys = {
            line.strip().split(":", 1)[0]: line.split(":", 1)[1].strip()
            for line in lines
            if re.match(r"^    [A-Za-z0-9_-]+:", line)
        }
        if "uses" in keys:
            # A reusable-workflow call has no runner; the called file is checked.
            if "runs-on" in keys:
                problems.append(f"{job}: reusable call must not set runs-on")
            continue
        if keys.get("runs-on") != routed(job):
            problems.append(f"{job}: runs-on is {keys.get('runs-on')!r}, not the routed expression")
    # Catch a runs-on hidden anywhere else (nested, flow style, odd indent).
    code = "\n".join(line for line in text.splitlines() if not line.lstrip().startswith("#"))
    expected = sum(1 for lines in jobs(text).values()
                   if not any(re.match(r"^    uses:", line) for line in lines))
    if len(re.findall(r"(?m)^\s*runs-on:", code)) != expected:
        problems.append("runs-on appears outside the job-level keys")
    if code.count("two-selfhosted") != expected:
        problems.append("a self-hosted label appears outside the routed expression")
    return problems


def workflow(job_id, runs_on, extra=""):
    return f"name: t\non:\n  push:\njobs:\n  {job_id}:\n    runs-on: {runs_on}\n    steps:\n      - run: true\n{extra}"


class RepositoryWorkflowTests(unittest.TestCase):
    def test_every_job_in_every_workflow_uses_the_routed_expression(self):
        files = sorted(WORKFLOWS.glob("*.y*ml"))
        self.assertTrue(files)
        routed_jobs = 0
        for path in files:
            text = path.read_text()
            self.assertEqual(violations(text), [], path.name)
            routed_jobs += text.count("'[\"self-hosted\",\"two-selfhosted\"]'")
        self.assertGreaterEqual(routed_jobs, 19)

    def test_required_check_jobs_and_their_needs_are_routed(self):
        check = jobs((WORKFLOWS / "check.yml").read_text())
        supply = jobs((WORKFLOWS / "supply-chain.yml").read_text())
        for job, lines in [*((j, check[j]) for j in ("check", "self-role-store", "parity-docs", "required-checks")),
                           *((j, supply[j]) for j in ("pr-lint", "gitleaks"))]:
            self.assertIn(f"    runs-on: {routed(job)}", lines, job)

    def test_public_repo_goes_hosted_before_any_variable_is_read(self):
        head = routed("x").split("||", 1)[0]
        self.assertEqual(head, "${{ fromJSON((!github.event.repository.private && '[\"ubuntu-latest\"]') ")
        self.assertNotIn("vars.", head)
        self.assertNotIn("self-hosted", head)


class CheckerMutationTests(unittest.TestCase):
    def test_canonical_job_passes(self):
        self.assertEqual(violations(workflow("build", routed("build"))), [])

    def test_reusable_workflow_call_needs_no_runner(self):
        text = workflow("build", routed("build"), "  bench:\n    uses: ./.github/workflows/b.yml\n")
        self.assertEqual(violations(text), [])

    def test_bare_self_hosted_label_fails(self):
        self.assertTrue(violations(workflow("build", "[self-hosted, two-selfhosted]")))

    def test_old_self_hosted_default_switch_fails(self):
        old = routed("build").replace(
            "(!github.event.repository.private && '[\"ubuntu-latest\"]') || ", "")
        self.assertTrue(violations(workflow("build", old)))

    def test_hard_coded_hosted_label_fails(self):
        # Hosted-only would bill private minutes if the repo goes private again.
        self.assertTrue(violations(workflow("build", "ubuntu-latest")))

    def test_another_jobs_switch_id_fails(self):
        self.assertTrue(violations(workflow("build", routed("check"))))

    def test_missing_runs_on_fails(self):
        text = "name: t\non:\n  push:\njobs:\n  build:\n    steps:\n      - run: true\n"
        self.assertTrue(violations(text))

    def test_nested_extra_label_fails(self):
        extra = "    strategy:\n      matrix:\n        runs-on: [self-hosted, two-selfhosted]\n"
        self.assertTrue(violations(workflow("build", routed("build"), extra)))

    def test_reusable_call_with_runner_fails(self):
        text = workflow("build", routed("build"),
                        "  bench:\n    uses: ./.github/workflows/b.yml\n    runs-on: ubuntu-latest\n")
        self.assertTrue(violations(text))


if __name__ == "__main__":
    unittest.main()
