"""Verify an offline native PRE ownership-fence transcript."""

from dataclasses import dataclass
from uuid import UUID

from pre_native_takeover import _MAX_EPOCH
from pre_start_stop import Deployment, EffectWindow, _valid_deployment
from pre_target import Target, require


STALE_EPOCH_REFUSAL = "epoch_conflict"  # Worker ownership control's epoch mismatch reason.


@dataclass(frozen=True)
class FenceTransition:
    kind: str
    window: EffectWindow
    deployment: Deployment
    epoch: int


@dataclass(frozen=True)
class StaleWriteProbe:
    window: EffectWindow
    deployment: Deployment
    epoch: int
    accepted: bool
    refusal_code: str
    mutations: int


@dataclass(frozen=True)
class EpochFenceVerdict:
    enabled: bool
    original_epoch: int | None = None
    takeover_epoch: int | None = None
    stop_fence_epoch: int | None = None
    stale_writes_refused: int = 0


_DISABLED = EpochFenceVerdict(enabled=False)


def _plain_target(target):
    return (type(target) is Target and type(target.shape) is str and type(target.scope) is str
            and type(target.run_id) is int and type(target.started_at) is int)


def _matches_window(observed, window):
    return (type(observed) is EffectWindow and _plain_target(observed.target)
            and type(observed.cutoff) is int and observed == window)


def _matches_reservation(deployment, target, start_id):
    return (_valid_deployment(deployment) and _plain_target(deployment.target)
            and deployment.target == target and deployment.start_id == start_id)


def verify_native_pre_epoch_fence(
    target: Target | None = None, *, enabled: bool = False, window: EffectWindow | None = None,
    start_id: UUID | None = None, original_deployment: Deployment | None = None,
    replacement_deployment: Deployment | None = None,
    observations: tuple[FenceTransition | StaleWriteProbe, ...] = (),
) -> EpochFenceVerdict:
    """Check caller-authenticated pins and an immutable transcript in observation order.

    Each transition records the resulting fence epoch. A probe records the
    writer epoch presented to ownership control, the Worker refusal reason,
    and the observed number of state mutations. Probes can follow any
    transition and use the latest fence at that point. Invalid evidence
    raises TargetError with a fixed reason.
    """
    if enabled is not True:
        return _DISABLED

    require(_plain_target(target), "invalid_pre_epoch_fence_target")
    target.__post_init__()
    require(type(window) is EffectWindow and _plain_target(window.target)
            and window.target == target, "wrong_pre_epoch_fence_window")
    window.__post_init__()
    require(type(start_id) is UUID and start_id.int, "invalid_pre_epoch_fence_start")
    require(_matches_reservation(original_deployment, target, start_id)
            and _matches_reservation(replacement_deployment, target, start_id),
            "wrong_pre_epoch_fence_deployment")
    require(original_deployment.worker_version != replacement_deployment.worker_version
            and original_deployment.container_id != replacement_deployment.container_id,
            "duplicate_pre_epoch_fence_deployment")
    require(type(observations) is tuple, "invalid_pre_epoch_fence_observations")

    transitions = (
        ("original_create", original_deployment),
        ("replacement_takeover", replacement_deployment),
        ("original_stop", original_deployment),
    )
    epochs = []
    refused = 0
    for observation in observations:
        require(type(observation) in (FenceTransition, StaleWriteProbe), "invalid_pre_epoch_fence_observation")
        require(_matches_window(observation.window, window), "wrong_pre_epoch_fence_window")
        deployment = observation.deployment
        require(_matches_reservation(deployment, target, start_id)
                and deployment in (original_deployment, replacement_deployment),
                "wrong_pre_epoch_fence_deployment")
        epoch = observation.epoch
        require(type(epoch) is int and 1 <= epoch <= _MAX_EPOCH, "invalid_pre_epoch_fence_epoch")

        if type(observation) is StaleWriteProbe:
            require(epochs and epoch < epochs[-1], "pre_epoch_fence_probe_not_stale")
            require(observation.accepted is False and type(observation.refusal_code) is str
                    and observation.refusal_code == STALE_EPOCH_REFUSAL
                    and type(observation.mutations) is int and observation.mutations == 0,
                    "pre_epoch_fence_stale_write_unconfirmed")
            refused += 1
            continue

        step = len(epochs)
        require(step < len(transitions) and type(observation.kind) is str
                and (observation.kind, deployment) == transitions[step], "wrong_pre_epoch_fence_sequence")
        if step == 1:
            require(epoch > epochs[-1], "pre_epoch_fence_takeover_not_advanced")
        if step == 2:
            require(epoch >= epochs[-1], "pre_epoch_fence_stop_rollback")
        epochs.append(epoch)

    require(len(epochs) == len(transitions), "incomplete_pre_epoch_fence_sequence")
    original, takeover, stop = epochs
    return EpochFenceVerdict(True, original, takeover, stop, refused)
