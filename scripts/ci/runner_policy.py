"""Stdlib-only runner pins shared by the offline workflow policy checks."""

PUBLIC_RUNNER = "ubuntu-latest"
RUST_TESTS_RUNNER = (
    "${{ github.repository == 'TogetherWeOwn/two-bot-next' "
    "&& toJSON(github.event.repository.private) == 'false' "
    "&& github.run_attempt == '1' "
    "&& (github.actor == 'Rick7C2' || github.actor == 'togetherweown[bot]') "
    "&& (github.triggering_actor == 'Rick7C2' || github.triggering_actor == 'togetherweown[bot]') "
    "&& github.event_name == 'pull_request' "
    "&& github.event.pull_request.head.repo.full_name == github.repository "
    "&& github.event.pull_request.base.ref == 'main' "
    "&& vars.CI_HEAVY_RUNNER == 'boat' && vars.CI_HEAVY_RUNNER || 'ubuntu-latest' }}"
)
IGNORED_DB_RUNTIME_RUNNER = (
    "${{ (github.event_name != 'pull_request' || "
    "github.event.pull_request.head.repo.full_name == github.repository) "
    "&& vars.CI_BLACKSMITH_2VCPU == 'blacksmith-2vcpu-ubuntu-2404' "
    "&& vars.CI_BLACKSMITH_2VCPU || 'ubuntu-latest' }}"
)
BLACKSMITH_2VCPU_SELECT = (
    "&& github.event.pull_request.head.repo.full_name == github.repository "
    "&& vars.CI_BLACKSMITH_2VCPU == 'blacksmith-2vcpu-ubuntu-2404' "
    "&& vars.CI_BLACKSMITH_2VCPU || 'ubuntu-latest'"
)
PR_GATE_RUNNER = "${{ github.event_name == 'pull_request' " + BLACKSMITH_2VCPU_SELECT + " }}"
PR_SIZE_GATE_RUNNER_INPUT = (
    "${{ toJSON((github.event_name == 'pull_request' || github.event_name == 'pull_request_review') "
    + BLACKSMITH_2VCPU_SELECT + ") }}"
)
CHECK_PR_GATES = {
    "check", "coverage-plan", "coverage-shard", "patch-coverage", "ignored-db-stores",
    "moderation-db", "parity-docs", "self-role-store", "job-inputs", "db-compatibility",
    "container-inputs", "community-db", "feeds-db", "tickets-postgres", "fuzz-compile",
    "worker", "ci-ok",
}
SUPPLY_CHAIN_PR_GATES = {"pr-lint", "gitleaks"}
CHECK_RUNNERS = {
    **dict.fromkeys(CHECK_PR_GATES, PR_GATE_RUNNER),
    "rust-tests": RUST_TESTS_RUNNER,
    "ignored-db-runtime": IGNORED_DB_RUNTIME_RUNNER,
}


def expected_runner(workflow, job_id):
    if workflow == "check.yml":
        return CHECK_RUNNERS.get(job_id, PUBLIC_RUNNER)
    if workflow == "supply-chain.yml" and job_id in SUPPLY_CHAIN_PR_GATES:
        return PR_GATE_RUNNER
    return PUBLIC_RUNNER
