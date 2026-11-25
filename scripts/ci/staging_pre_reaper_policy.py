"""Source policy for the dormant, synthetic-only PRE sweep."""

from copy import deepcopy


PRE_REAPER = "staging-pre-reaper.yml"
PRE_REAPER_QUEUE = "staging-pre-reaper"


def dormant_pre_reaper_errors(workflows):
    """Pin the dormant staging-only cleanup entry."""
    guard = (
        "github.repository_id == '1396224242' && "
        "github.ref == 'refs/heads/main' && github.ref_protected && "
        "((github.event_name == 'workflow_dispatch' && "
        "github.workflow_ref == 'TogetherWeOwn/two-bot-next/.github/workflows/"
        "staging-pre-qualification.yml@refs/heads/main') || "
        "(github.event_name == 'schedule' && "
        "github.workflow_ref == 'TogetherWeOwn/two-bot-next/.github/workflows/"
        "staging-pre-periodic.yml@refs/heads/main'))"
    )
    queue = PRE_REAPER_QUEUE
    expected = {
        "name": queue,
        "on": {"workflow_call": ""},
        "permissions": {},
        "jobs": {"sweep": {
            "if": guard,
            "environment": {"name": "staging", "deployment": "false"},
            "concurrency": {"group": queue, "cancel-in-progress": "false", "queue": "max"},
            "permissions": {"contents": "read"},
            "runs-on": "ubuntu-latest",
            "timeout-minutes": "5",
            "steps": [
                {"uses": "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
                 "with": {"ref": "${{ github.sha }}", "persist-credentials": "false", "fetch-depth": "1"}},
                {"name": "Exercise the PRE sweep against synthetic inventory",
                 "run": "python3 -B -m unittest scripts.ci.test_staging_pre_reaper_fixture"},
                {"name": "Signal unverified PRE cleanup", "if": "failure() || cancelled()",
                 "run": "echo '::error title=PRE cleanup incident::Cleanup could not be verified. "
                        "Inspect the staging PRE sweep run.'"},
            ],
        }},
    }
    actual = deepcopy(workflows.get(PRE_REAPER, {}))
    sweep = actual.get("jobs", {}).get("sweep", {})
    if "if" in sweep:
        sweep["if"] = " ".join(sweep["if"].split())
    errors = []
    if actual != expected:
        errors.append(f"{PRE_REAPER}: expected the protected, synthetic-only sweep and static incident signal")
    for name, workflow in workflows.items():
        if name == PRE_REAPER:
            continue
        for job in workflow.get("jobs", {}).values():
            path = job.get("uses", "").partition("@")[0].lower()
            if path.endswith(f"/{PRE_REAPER}"):
                errors.append(f"{name}: the PRE reaper must remain unwired")
    return errors
