"""Stdlib-only runner pins shared by the offline workflow policy checks."""

PUBLIC_RUNNER = "ubuntu-latest"
RUST_TESTS_RUNNER = (
    "${{ github.repository == 'TogetherWeOwn/two-bot-next' "
    "&& toJSON(github.event.repository.private) == 'false' "
    "&& github.run_attempt == '1' "
    "&& (github.actor == 'Rick7C2' || github.actor == 'togetherweown[bot]') "
    "&& (github.triggering_actor == 'Rick7C2' || github.triggering_actor == 'togetherweown[bot]') "
    "&& ((github.event_name == 'pull_request' "
    "&& github.event.pull_request.head.repo.full_name == github.repository "
    "&& github.event.pull_request.base.ref == 'main') "
    "|| (github.event_name == 'push' && github.ref == 'refs/heads/main' "
    "&& toJSON(github.ref_protected) == 'true')) "
    "&& vars.CI_HEAVY_RUNNER == 'boat' && 'boat' || 'ubuntu-latest' }}"
)


def expected_runner(workflow, job_id):
    if (workflow, job_id) == ("check.yml", "rust-tests"):
        return RUST_TESTS_RUNNER
    return PUBLIC_RUNNER
