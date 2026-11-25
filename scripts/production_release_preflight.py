"""Check the approved attendance release and its live recovery pair before fencing."""

from dataclasses import asdict, dataclass
import re

from production_migrate_approval import stable_plan
from production_recovery_takeover import HEX, SHA, UUID
from production_release_forward import ledger_endpoints
from staging_migrate_approval import digest_json
from staging_recovery_status import DIGEST, matches
from staging_rollout import mapping, require

NAMESPACE = re.compile(r"[0-9a-f]{32}")


@dataclass(frozen=True)
class ReleasePreflight:
    workflow_sha: str
    candidate_sha: str
    candidate_archive: str
    candidate_image: str
    rollback_sha: str
    rollback_worker: str
    rollback_image: str
    application_id: str
    namespace_id: str
    restore_proof_sha256: str

    def recovery_reference(self):
        fields = (
            (SHA, self.workflow_sha), (SHA, self.candidate_sha), (SHA, self.rollback_sha),
            (UUID, self.rollback_worker), (UUID, self.application_id), (NAMESPACE, self.namespace_id),
            (DIGEST, self.candidate_archive), (DIGEST, self.candidate_image), (DIGEST, self.rollback_image),
            (HEX, self.restore_proof_sha256),
        )
        require(all(matches(pattern, value) for pattern, value in fields)
                and self.candidate_sha != self.rollback_sha and self.candidate_image != self.rollback_image,
                "production_release_preflight_pins_invalid")
        # The provider assigns a new Worker version when it uploads the
        # approved modules. Bind the immutable package before that upload.
        return "inverse-" + digest_json({"operation": "attendance-0480", **asdict(self)})


def verify_preflight(pin, target, plan, fresh, observe, control):
    """Verify a fresh PRE plan and the exact live pair's read-only takeover capability."""
    reference = pin.recovery_reference()
    require(plan.get("mode") == "plan" and plan.get("role") == "two_bot_migrator_ro"
            and plan.get("migration_target") == "production" and plan.get("target") == target
            and plan.get("source_sha") == pin.candidate_sha and plan.get("recovery_evidence_ref") == reference
            and stable_plan(plan) == stable_plan(fresh), "production_release_preflight_plan_changed")
    ledger_endpoints(plan)
    state = observe()
    worker, image, owner = (mapping(state.get(key)) for key in ("worker", "container", "ownership"))
    require(worker.get("version_id") == pin.rollback_worker and worker.get("sha") == pin.rollback_sha
            and worker.get("traffic_percent") == 100 and image.get("image_digest") == pin.rollback_image
            and image.get("application_id") == pin.application_id and image.get("namespace_id") == pin.namespace_id
            and image.get("rollout_status") == "completed" and owner.get("phase") == "active"
            and owner.get("running") is True and type(owner.get("epoch")) is int and 0 < owner["epoch"] < 2**53 - 2,
            "production_release_retained_pair_changed")
    capability = control("takeover-capability", expected_deployment=pin.rollback_worker)
    require(capability == {"deploymentId": pin.rollback_worker, "exactTakeover": True}
            and capability["exactTakeover"] is True, "production_release_retained_takeover_unproved")
    after = observe()
    require(all(after.get(key) == state[key] for key in ("worker", "container", "ownership")),
            "production_release_retained_pair_changed")
    return owner["epoch"]
