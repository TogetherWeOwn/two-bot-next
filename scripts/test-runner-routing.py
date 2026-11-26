"""Pin exact runner routing in this public repository.

Ordinary jobs use ubuntu-latest, with a narrowly scoped Blacksmith route.
A private conversion requires fresh runner and cost qualification before
execution. Offline and stdlib-only: no GitHub
credentials, no PyYAML. Visibility is verified separately before execution.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github/workflows"
PUBLIC_RUNNER = "ubuntu-latest"
IGNORED_DB_RUNTIME_RUNNER = (
    "${{ (github.event_name != 'pull_request' || "
    "github.event.pull_request.head.repo.full_name == github.repository) "
    "&& vars.CI_BLACKSMITH_2VCPU == 'blacksmith-2vcpu-ubuntu-2404' "
    "&& 'blacksmith-2vcpu-ubuntu-2404' || 'ubuntu-latest' }}"
)
# Negative fixture: the former public-first branch still collected private
# self-hosted/overflow routing in source, even when it was never selected.
LEGACY_ROUTED = (
    "${{ fromJSON((!github.event.repository.private && '[\"ubuntu-latest\"]') || "
    "(contains(fromJSON(vars.CI_OVERFLOW_JOBS || '[]'), '{job}') && "
    "contains(fromJSON(vars.CI_OVERFLOW_EVENTS || '[]'), github.event_name) && "
    "vars.CI_OVERFLOW_RUNNER) || '[\"self-hosted\",\"two-selfhosted\"]') }}"
)


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


def violations(text, workflow_name=None):
    """Reject unapproved runners, including dormant routing."""
    problems = []
    for job, lines in jobs(text).items():
        keys = {
            line.strip().split(":", 1)[0]: line.split(":", 1)[1].strip()
            for line in lines
            if re.match(r"^    [A-Za-z0-9_-]+:", line)
        }
        if workflow_name == "voice-rest-measurement.yml" and job in ("voice-first", "voice-second"):
            if keys.get("uses", "").split(" #", 1)[0] != "./.github/workflows/voice-synthetic.yml":
                problems.append(f"{job}: reusable target changed")
        if "uses" in keys:
            # A reusable-workflow call has no runner; the called file is checked.
            if "runs-on" in keys:
                problems.append(f"{job}: reusable call must not set runs-on")
            continue
        expected = PUBLIC_RUNNER
        if (workflow_name, job) == ("check.yml", "ignored-db-runtime"):
            expected = IGNORED_DB_RUNTIME_RUNNER
        if keys.get("runs-on") != expected:
            problems.append(f"{job}: expected runs-on {expected!r}, got {keys.get('runs-on')!r}")
    # Catch a runs-on hidden anywhere else (nested, flow style, odd indent).
    code = "\n".join(line for line in text.splitlines() if not line.lstrip().startswith("#"))
    expected = sum(1 for lines in jobs(text).values()
                   if not any(re.match(r"^    uses:", line) for line in lines))
    if len(re.findall(r"(?m)^\s*runs-on:", code)) != expected:
        problems.append("runs-on appears outside the job-level keys")
    for forbidden in ("self-hosted", "two-selfhosted", "CI_OVERFLOW_"):
        if forbidden in code:
            problems.append(f"dormant runner routing remains: {forbidden}")
    return problems


def workflow(job_id, runs_on, extra=""):
    return f"name: t\non:\n  push:\njobs:\n  {job_id}:\n    runs-on: {runs_on}\n    steps:\n      - run: true\n{extra}"


class RepositoryWorkflowTests(unittest.TestCase):
    def test_every_job_in_every_workflow_uses_explicit_public_hosted_runner(self):
        files = sorted(WORKFLOWS.glob("*.y*ml"))
        self.assertTrue(files)
        hosted_jobs = 0
        for path in files:
            text = path.read_text()
            self.assertEqual(violations(text, path.name), [], path.name)
            hosted_jobs += text.count(f"    runs-on: {PUBLIC_RUNNER}")
        self.assertGreaterEqual(hosted_jobs, 88)

    def test_required_check_jobs_and_their_needs_are_hosted(self):
        check = jobs((WORKFLOWS / "check.yml").read_text())
        supply = jobs((WORKFLOWS / "supply-chain.yml").read_text())
        for job, lines in [*((j, check[j]) for j in ("check", "self-role-store", "parity-docs", "container-inputs", "ci-ok")),
                           *((j, supply[j]) for j in ("pr-lint", "gitleaks"))]:
            self.assertIn(f"    runs-on: {PUBLIC_RUNNER}", lines, job)


class CheckerMutationTests(unittest.TestCase):
    def test_blacksmith_route_is_exact_and_scoped_to_ignored_db_runtime(self):
        canonical = workflow("ignored-db-runtime", IGNORED_DB_RUNTIME_RUNNER)
        self.assertEqual(violations(canonical, "check.yml"), [])
        self.assertTrue(violations(canonical, "nightly.yml"))
        self.assertTrue(violations(workflow("coverage-shard", IGNORED_DB_RUNTIME_RUNNER), "check.yml"))
        self.assertTrue(violations(workflow("ignored-db-runtime", PUBLIC_RUNNER), "check.yml"))
        for mutated in (
            canonical.replace("github.event.pull_request.head.repo.full_name == github.repository", "true"),
            canonical.replace("CI_BLACKSMITH_2VCPU", "CI_BLACKSMITH_4VCPU"),
            canonical.replace(" == 'blacksmith-2vcpu-ubuntu-2404' && 'blacksmith-2vcpu-ubuntu-2404'", ""),
            canonical.replace("|| 'ubuntu-latest'", ""),
        ):
            with self.subTest(mutated=mutated):
                self.assertTrue(violations(mutated, "check.yml"))

    def test_canonical_job_passes(self):
        self.assertEqual(violations(workflow("build", PUBLIC_RUNNER)), [])

    def test_reusable_workflow_call_needs_no_runner(self):
        text = workflow("build", PUBLIC_RUNNER, "  bench:\n    uses: ./.github/workflows/b.yml\n")
        self.assertEqual(violations(text), [])

    def test_bare_self_hosted_label_fails(self):
        self.assertTrue(violations(workflow("build", "[self-hosted, two-selfhosted]")))

    def test_legacy_public_private_overflow_route_fails(self):
        self.assertTrue(violations(workflow("build", LEGACY_ROUTED.replace("{job}", "build"))))

    def test_old_self_hosted_default_switch_fails(self):
        old = LEGACY_ROUTED.replace("{job}", "build").replace(
            "(!github.event.repository.private && '[\"ubuntu-latest\"]') || ", "")
        self.assertTrue(violations(workflow("build", old)))

    def test_indirect_or_nonstandard_hosted_runner_fails(self):
        for runs_on in ("${{ vars.RUNNER }}", "${{ matrix.runner }}", "ubuntu-latest-16-cores",
                        "windows-latest", "[ubuntu-latest]", "[ubuntu-latest, self-hosted]"):
            with self.subTest(runs_on=runs_on):
                self.assertTrue(violations(workflow("build", runs_on)))

    def test_dormant_overflow_setting_fails(self):
        text = workflow("build", PUBLIC_RUNNER, "    env:\n      OLD_RUNNER: ${{ vars.CI_OVERFLOW_RUNNER }}\n")
        self.assertTrue(violations(text))

    def test_nightly_partitions_use_explicit_public_hosted_runner(self):
        for job in ("sweep-plan", "sweep-partition", "sweep-guarded", "sweep-docs"):
            with self.subTest(job=job):
                self.assertEqual(violations(workflow(job, PUBLIC_RUNNER)), [])
                self.assertTrue(violations(workflow(job, LEGACY_ROUTED.replace("{job}", "sweep"))))
                self.assertTrue(violations(workflow(job, LEGACY_ROUTED.replace("{job}", job))))
                self.assertTrue(violations(workflow(job, LEGACY_ROUTED.replace("{job}", "check"))))

    def test_all_workflows_reject_former_nightly_sweep_aliases(self):
        for job in ("sweep-plan", "sweep-partition", "sweep-guarded", "sweep-docs", "sweep-extra"):
            with self.subTest(job=job):
                self.assertTrue(violations(workflow(job, LEGACY_ROUTED.replace("{job}", "sweep"))))
                self.assertEqual(violations(workflow(job, PUBLIC_RUNNER)), [])

    def test_missing_runs_on_fails(self):
        text = "name: t\non:\n  push:\njobs:\n  build:\n    steps:\n      - run: true\n"
        self.assertTrue(violations(text))

    def test_nested_extra_label_fails(self):
        extra = "    strategy:\n      matrix:\n        runs-on: [self-hosted, two-selfhosted]\n"
        self.assertTrue(violations(workflow("build", PUBLIC_RUNNER, extra)))

    def test_reusable_call_with_runner_fails(self):
        text = workflow("build", PUBLIC_RUNNER,
                        "  bench:\n    uses: ./.github/workflows/b.yml\n    runs-on: ubuntu-latest\n")
        self.assertTrue(violations(text))


if __name__ == "__main__":
    unittest.main()
