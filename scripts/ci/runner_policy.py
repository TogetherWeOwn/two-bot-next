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
CHECK_RUNNERS = {"rust-tests": RUST_TESTS_RUNNER, "ignored-db-runtime": IGNORED_DB_RUNTIME_RUNNER}


def expected_runner(workflow, job_id):
    if workflow == "check.yml":
        return CHECK_RUNNERS.get(job_id, PUBLIC_RUNNER)
    return PUBLIC_RUNNER
