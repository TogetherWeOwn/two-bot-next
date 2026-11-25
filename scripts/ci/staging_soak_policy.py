"""The split soak's reviewed graph and compatibility view for lifecycle assertions."""
import json
from pathlib import Path

SOAK = "staging-soak.yml"
PRODUCTION_RELEASE_GROUP = (
    "${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' && github.run_attempt == 1 "
    "&& (inputs.release_plan_run_id != '' || inputs.release_recovery_run_id != '') && 'deploy-production' || "
    "format('deploy-production-run-{0}-{1}', github.run_id, github.run_attempt) }}"
)
PRODUCTION_GROUP = (
    "${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' && github.run_attempt == 1 "
    "&& inputs.release_plan_run_id != '' && format('deploy-production-release-job-{0}', github.run_id) "
    "|| github.run_attempt != 1 && needs.guard.outputs.mode != 'rollback' && needs.guard.outputs.allow_unstaged != 'true' "
    "&& format('deploy-production-refused-{0}-{1}', github.run_id, github.run_attempt) || 'deploy-production' }}"
)
CONTRACT = json.loads((Path(__file__).parent / "fixtures/staging_soak_contract.json").read_text())


def admitted_soak(workflows):
    return {"caller": workflows.get("deploy-production.yml", {}).get("jobs", {}).get("soak"),
            "reusable": workflows.get(SOAK)} == CONTRACT


def soak_view(workflows):
    begin, finish = (workflows[SOAK]["jobs"][job] for job in ("begin", "finish"))
    caller = workflows["deploy-production.yml"]["jobs"]["soak"]
    steps = json.loads(json.dumps(begin["steps"][1:5] + finish["steps"][-5:]).replace(
        "inputs.", "needs.guard.outputs.").replace("needs.begin.outputs.", "steps.staging-use-begin.outputs."))
    steps[4]["env"]["FULL_SOAK"] = caller["with"]["full_soak"]
    steps[6]["env"].pop("FULL_SOAK")
    for field in ("FULL_SOAK", "STAGING_USE_SHA", "STAGING_USE_DEPLOYMENT"):
        del steps[7]["env"][field]
    steps[7]["run"] = steps[7]["run"].splitlines()[0]  # Release proof pinned separately.
    return {**finish, "needs": caller["needs"], "if": caller["if"], "steps": steps,
            "concurrency": {"group": "staging-lifecycle", "cancel-in-progress": "false", "queue": "max"},
            "outputs": {key: "${{ steps.judge.outputs." + key + " }}" for key in finish["outputs"]}}
