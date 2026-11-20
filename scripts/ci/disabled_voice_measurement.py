"""Admission predicate for the inert voice measurement scaffold.

This predicate does not authorize an active staging caller. Activation needs
its own lifecycle-lock and consumer-policy review.
"""

WORKFLOW = "voice-rest-measurement.yml"
FIRST = "github.ref == 'refs/heads/main' && github.run_attempt == 1"
QUEUE = {
    "group": "${{ false && github.event_name == 'workflow_dispatch' && " + FIRST
    + " && 'staging-lifecycle' || format('voice-rest-measurement-disabled-{0}-{1}', github.run_id, github.run_attempt) }}",
    "cancel-in-progress": "false",
    "queue": "max",
}
GUARDS = {name: "${{ false && " + condition + " }}" for name, condition in {
    "staging-window": FIRST,
    "measurement-before": FIRST + " && needs.staging-window.result == 'success'",
    "voice-first": FIRST + " && needs.staging-window.result == 'success' && needs.measurement-before.result == 'success'",
    "measurement-after": FIRST + " && needs.voice-first.result == 'success'",
    "voice-second": FIRST + " && needs.measurement-after.result == 'success' && needs.measurement-after.outputs.second_required == 'true'",
    "measurement-final": FIRST + " && needs.voice-second.result == 'success'",
    "staging-use-release": "always() && " + FIRST + " && needs.staging-window.outputs.expires_at != ''",
    "measurement-terminal": "always() && " + FIRST,
    "rerun-refusal": "always() && github.run_attempt != 1",
}.items()}


def is_disabled_scaffold(workflow):
    """Accept only the exact inert job graph and its isolated queue."""
    if not isinstance(workflow, dict):
        return False
    jobs = workflow.get("jobs")
    if not isinstance(jobs, dict) or set(jobs) != set(GUARDS):
        return False
    triggers = workflow.get("on")
    if not isinstance(triggers, dict) or set(triggers) != {"workflow_dispatch"}:
        return False
    if workflow.get("permissions") != {} or workflow.get("concurrency") != QUEUE:
        return False
    return all(isinstance(jobs[name], dict) and jobs[name].get("if") == guard
               and "concurrency" not in jobs[name] for name, guard in GUARDS.items())
