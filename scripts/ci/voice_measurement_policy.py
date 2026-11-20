"""Admission contract for the staging-only voice REST measurement caller."""

WORKFLOW = "voice-rest-measurement.yml"
TARGET = "./.github/workflows/voice-synthetic.yml"
CALLS = ("voice-first", "voice-second")
STAGING_JOBS = {"staging-window", "measurement-before", "measurement-after",
                "measurement-final", "staging-use-release"}
FIRST = "github.ref == 'refs/heads/main' && github.ref_protected && github.run_attempt == 1"
GUARDS = {name: "${{ " + condition + " }}" for name, condition in {
    "staging-window": FIRST,
    "measurement-before": FIRST + " && needs.staging-window.result == 'success'",
    "voice-first": FIRST + " && needs.staging-window.result == 'success' && needs.measurement-before.result == 'success'",
    "measurement-after": FIRST + " && needs.voice-first.result == 'success'",
    "voice-second": FIRST + " && needs.measurement-after.result == 'success' && needs.measurement-after.outputs.second_required == 'true'",
    "measurement-final": FIRST + " && needs.voice-second.result == 'success'",
    "staging-use-release": "always() && " + FIRST + " && needs.staging-window.outputs.expires_at != ''",
    "measurement-terminal": "always() && " + FIRST,
    "rerun-refusal": "always() && !(" + FIRST + ")",
}.items()}


def measurement_queue(canonical):
    return {
        "group": "${{ github.event_name == 'workflow_dispatch' && " + FIRST
        + f" && '{canonical}' || "
        "format('voice-rest-measurement-refused-{0}-{1}', github.run_id, github.run_attempt) }}",
        "cancel-in-progress": "false",
        "queue": "max",
    }


def is_measurement_caller(workflow, canonical):
    """Admit only the exact dispatch, lock, guards and staging authority."""
    if not isinstance(workflow, dict) or workflow.get("on") != {"workflow_dispatch": ""}:
        return False
    if workflow.get("permissions") != {} or workflow.get("concurrency") != measurement_queue(canonical):
        return False
    if {"env", "defaults"} & workflow.keys():
        return False
    jobs = workflow.get("jobs")
    if not isinstance(jobs, dict) or set(jobs) != set(GUARDS):
        return False
    grant = {
        "deploy_staging_call": "true",
        "staging_use_expires_at": "${{ needs.staging-window.outputs.expires_at }}",
        "staging_use_sha": "${{ needs.staging-window.outputs.sha }}",
        "staging_use_deployment": "${{ needs.staging-window.outputs.deployment_id }}",
    }
    return all(isinstance(job, dict) and job.get("if") == GUARDS[name]
               and not {"concurrency", "env", "defaults", "continue-on-error"} & job.keys()
               and job.get("environment") == ("staging" if name in STAGING_JOBS else None)
               and job.get("uses") == (TARGET if name in CALLS else None)
               and job.get("permissions") == ({} if name == "rerun-refusal" else {"contents": "read"})
               and (name not in CALLS or job.get("with") == dict(grant, artifact_invocation=name.removeprefix("voice-")))
               for name, job in jobs.items())
