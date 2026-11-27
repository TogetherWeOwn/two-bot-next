"""Legacy takeover after the protected database inverse.

The protected workflow must authenticate the database result and provide
an independent bracketed ownership/provider read. This module is a
library for that workflow, with no dispatch or command-line entry point.
"""

from dataclasses import asdict, dataclass
import re

from staging_rollout import GateError, require

HEX = re.compile(r"[0-9a-f]{64}")
SHA = re.compile(r"[0-9a-f]{40}")
UUID = re.compile(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}")
RUN_ID = re.compile(r"[1-9][0-9]{0,19}")
# This reviewed predecessor has epoch-fenced control but predates version-bound POSTs.
LEGACY_RELEASE_WORKER_SHA = "d314f4c63ecbea263aa953bc8a649a444a97da7c"


def retained_takeover_action(rollback_sha):
    return "takeover-legacy-fenced" if rollback_sha == LEGACY_RELEASE_WORKER_SHA else "takeover-fenced"


def require_same_provider_pair(before, after):
    require(all(after.get(key) == before[key] for key in ("worker", "container")),
            "production_recovery_pair_changed")


@dataclass(frozen=True)
class RecoveryPin:
    run_id: str
    run_attempt: int
    candidate_sha: str
    rollback_sha: str
    worker_version: str
    image_digest: str
    application_id: str
    namespace_id: str
    database_fingerprint: str
    expanded_ledgers_sha256: str
    restored_ledgers_sha256: str
    attendance_fence_sha256: str
    fence_epoch: int

    @property
    def actor(self):
        return f"github-actions:{self.run_id}:{self.rollback_sha}"


def verify_inverse(pin, authenticated_readback):
    """Match a result authenticated by the protected caller to its own pins."""
    require(isinstance(pin, RecoveryPin), "production_recovery_pin_invalid")
    fields = (
        (RUN_ID, pin.run_id), (SHA, pin.candidate_sha), (SHA, pin.rollback_sha),
        (UUID, pin.worker_version), (UUID, pin.application_id),
        (re.compile(r"[0-9a-f]{32}"), pin.namespace_id),
        (re.compile(r"sha256:[0-9a-f]{64}"), pin.image_digest),
        (HEX, pin.database_fingerprint), (HEX, pin.expanded_ledgers_sha256),
        (HEX, pin.restored_ledgers_sha256), (HEX, pin.attendance_fence_sha256),
    )
    require(all(isinstance(value, str) and pattern.fullmatch(value) for pattern, value in fields)
            and type(pin.run_attempt) is int and pin.run_attempt == 1
            and type(pin.fence_epoch) is int and 0 < pin.fence_epoch < 2**53 - 1
            and pin.candidate_sha != pin.rollback_sha
            and pin.expanded_ledgers_sha256 != pin.restored_ledgers_sha256,
            "production_recovery_pin_invalid")
    expected = {"state": "restored", **asdict(pin)}
    require(isinstance(authenticated_readback, dict)
            and set(authenticated_readback) == set(expected)
            and all(type(authenticated_readback[key]) is type(value)
                    and authenticated_readback[key] == value for key, value in expected.items()),
            "production_inverse_readback_mismatch")


def observed_phase(pin, observe):
    # Reuse the protected production ownership status projection. Its
    # provider/ownership bracket includes the single 100%-serving version.
    try:
        state = observe()
        worker, image, owner = state["worker"], state["container"], state["ownership"]
        require(worker["version_id"] == pin.worker_version and worker["sha"] == pin.rollback_sha
                and type(worker["traffic_percent"]) is int and worker["traffic_percent"] == 100
                and image["application_id"] == pin.application_id and image["namespace_id"] == pin.namespace_id
                and image["image_digest"] == pin.image_digest and image["rollout_status"] == "completed",
                "production_recovery_pair_changed")
        require(type(owner["epoch"]) is int and type(owner["running"]) is bool,
                "production_recovery_fence_changed")
        if owner["phase"] == "fenced" and owner["epoch"] == pin.fence_epoch and not owner["running"]:
            return "fenced"
        if (owner["phase"] == "active" and owner["epoch"] == pin.fence_epoch + 1
                and owner["owner_actor"] == pin.actor):
            return "active"
        if (owner["phase"] == "fenced" and owner["epoch"] == pin.fence_epoch + 1
                and owner["owner_actor"] == pin.actor and not owner["running"]):
            raise GateError("production_recovery_takeover_pending_reconcile")
        raise GateError("production_recovery_fence_changed")
    except (KeyError, TypeError):
        raise GateError("production_recovery_observation_invalid") from None


def complete_legacy_takeover(pin, authenticated_readback, observe, control):
    """Take over once at the recovered epoch, or resume an acknowledged result.

    The caller still runs the normal stable gateway-readiness gate. No
    provider mutation or readiness probe takes place on a failed readback.
    """
    verify_inverse(pin, authenticated_readback)
    before = observe()
    if observed_phase(pin, lambda: before) == "active":
        return "resumed"

    def confirmed():
        after = observe()
        require_same_provider_pair(before, after)
        return observed_phase(pin, lambda: after) == "active"

    try:
        control(retained_takeover_action(pin.rollback_sha), pin.fence_epoch, expected_deployment=pin.worker_version)
    except GateError:
        # A lost reply can commit. Confirm the same pair and next epoch without another POST.
        if not confirmed():
            raise
        return "resumed"
    require(confirmed(), "production_recovery_takeover_unconfirmed")
    return "taken-over"
