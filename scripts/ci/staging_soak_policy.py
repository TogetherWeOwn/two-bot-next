"""The split soak's reviewed graph and compatibility view for lifecycle assertions."""
import json
from pathlib import Path

SOAK = "staging-soak.yml"
PRODUCTION = "deploy-production.yml"
CONTRACT = json.loads((Path(__file__).parent / "fixtures/staging_soak_contract.json").read_text())


def admitted_soak(workflows):
    return {"caller": workflows.get(PRODUCTION, {}).get("jobs", {}).get("soak"),
            "reusable": workflows.get(SOAK)} == CONTRACT


def soak_view(workflows):
    """Expose the ordered lifecycle steps to the existing whole-window tests."""
    begin, finish = (workflows[SOAK]["jobs"][job] for job in ("begin", "finish"))
    caller = workflows[PRODUCTION]["jobs"]["soak"]
    local = lambda value: json.loads(json.dumps(value).replace("inputs.", "needs.guard.outputs.")
                                     .replace("needs.begin.outputs.", "steps.staging-use-begin.outputs."))
    steps = local(begin["steps"][1:5] + finish["steps"][-5:])
    steps[4]["env"]["FULL_SOAK"] = caller["with"]["full_soak"]
    steps[6]["env"].pop("FULL_SOAK")
    for field in ("FULL_SOAK", "STAGING_USE_SHA", "STAGING_USE_DEPLOYMENT"):
        del steps[7]["env"][field]
    steps[7]["run"] = steps[7]["run"].splitlines()[0]  # Release proof pinned separately.
    return {**finish, "needs": caller["needs"], "if": caller["if"], "steps": steps,
            "concurrency": {"group": "staging-lifecycle", "cancel-in-progress": "false", "queue": "max"},
            "outputs": {key: "${{ steps.judge.outputs." + key + " }}" for key in finish["outputs"]}}
