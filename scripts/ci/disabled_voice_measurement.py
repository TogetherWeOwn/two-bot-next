"""Admission predicate for the inert voice measurement scaffold.

This predicate does not authorize an active staging caller. Activation needs
its own lifecycle-lock and consumer-policy review.
"""

WORKFLOW = "voice-rest-measurement.yml"
GUARDS = {
    "staging-window": "${{ false && github.ref == 'refs/heads/main' }}",
    "measurement-before": "${{ false && github.ref == 'refs/heads/main' && needs.staging-window.result == 'success' }}",
    "voice-first": "${{ false && github.ref == 'refs/heads/main' && needs.staging-window.result == 'success' && needs.measurement-before.result == 'success' }}",
    "measurement-after": "${{ false && github.ref == 'refs/heads/main' && needs.voice-first.result == 'success' }}",
    "staging-use-release": "${{ false && always() && github.ref == 'refs/heads/main' && needs.staging-window.outputs.expires_at != '' }}",
}


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
    if workflow.get("permissions") != {} or workflow.get("concurrency") != {
        "group": "voice-rest-measurement-disabled", "cancel-in-progress": "false"
    }:
        return False
    return all(isinstance(jobs[name], dict) and jobs[name].get("if") == guard
               for name, guard in GUARDS.items())
