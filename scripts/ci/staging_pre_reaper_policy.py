"""Source policy for the dormant, synthetic-only PRE sweep."""

import hashlib
import json

PRE_REAPER = "staging-pre-reaper.yml"
PRE_REAPER_QUEUE = "staging-pre-reaper"

# Pin the parsed trigger, caller, job, queue, grants and fixture command together.
DORMANT_GRAPH = "53d6d2e794272df2c5d44aef3d653789b20cf79e358e20ad26ad40a1612b58d0"


def dormant_pre_reaper_errors(workflows):
    errors = []
    workflow = workflows.get(PRE_REAPER, {})
    graph = hashlib.sha256(json.dumps(workflow, sort_keys=True).encode()).hexdigest()
    if graph != DORMANT_GRAPH:
        errors.append(f"{PRE_REAPER}: expected the protected, synthetic-only sweep and static incident signal")
    for name, workflow in workflows.items():
        if name == PRE_REAPER:
            continue
        for job in workflow.get("jobs", {}).values():
            path = job.get("uses", "").partition("@")[0].lower()
            if path.endswith(f"/{PRE_REAPER}"):
                errors.append(f"{name}: the PRE reaper must remain unwired")
    return errors
