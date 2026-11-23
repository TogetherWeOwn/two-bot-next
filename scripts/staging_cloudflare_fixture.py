"""Offline Cloudflare readbacks for staging gate tests."""

from copy import deepcopy
from itertools import repeat


# Observed (active, healthy) pairs for a completed singleton rollout.
LAG_COUNTERS = ((0, 1), (1, 0))


def stale_reads(previous, reads):
    """Keep an old snapshot for N reads. None holds it indefinitely."""
    snapshot = deepcopy(previous)
    return repeat(snapshot) if reads is None else repeat(snapshot, reads)


class CloudflareFake:
    """Model independent Worker, container control plane and runtime views."""

    def __init__(self, history, application=None, rollouts=(), *, split_versions=()):
        self.history = list(history)
        self.application = deepcopy(application)
        self.rollouts = deepcopy(list(rollouts))
        self.serving_image = application["configuration"]["image"] if application else None
        self.split_versions = split_versions
        self._deployments = iter(())
        self._application_listing = iter(())
        self._rollout_listing = iter(())
        self._runtime = iter(())

    def deploy_worker(self, version, *, deployment_reads=0):
        self._deployments = stale_reads(self.history, deployment_reads)
        self.history.insert(0, version)

    def deployments(self):
        rows = []
        for version in next(self._deployments, self.history):
            traffic = [(version, 100)] if version else [(v, 50) for v in self.split_versions]
            rows.append({"id": "d", "versions": [
                {"version_id": v, "percentage": percent} for v, percent in traffic]})
        return {"deployments": rows}

    def start_rollout(self, row, *, application_reads=0, rollout_list_reads=0,
                      previous_probes=0, counters=None):
        self._application_listing = stale_reads(self.serving_image, application_reads)
        self._rollout_listing = stale_reads(self.rollouts, rollout_list_reads)
        self._runtime = stale_reads(self.serving_image, previous_probes)
        row = deepcopy(row)
        row["current_configuration"]["image"] = self.serving_image
        if counters is not None:
            active, healthy = counters
            row["health"]["instances"].update(active=active, healthy=healthy)
        self.rollouts.append(row)
        self.serving_image = row["target_configuration"]["image"]

    def application_listing(self):
        app = deepcopy(self.application)
        app["configuration"]["image"] = next(self._application_listing, self.serving_image)
        return [app]

    def rollout_listing(self):
        return deepcopy(next(self._rollout_listing, self.rollouts))

    def rollout_detail(self, rollout_id):
        for row in self.rollouts:
            if row["id"] == rollout_id:
                return deepcopy(row)
        raise KeyError(rollout_id)

    def ready_image(self):
        return next(self._runtime, self.serving_image)

    def replace_container(self):
        """Release the old process after the gate's fenced stop."""
        self._runtime = iter(())
