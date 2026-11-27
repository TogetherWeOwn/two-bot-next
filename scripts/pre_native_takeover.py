"""Dormant exact ownership takeover for an authenticated replacement PRE."""

from collections.abc import Callable
import re
from typing import Protocol
from uuid import UUID

from pre_ownership import CONTROL_PATH, CONTROL_REPLY_CAP, OwnershipReply, OwnershipTransport
from pre_start_stop import Deployment, EffectWindow
from pre_target import Target, TargetError, require
from script_intake import decode_json


_UNCONFIRMED = "pre_native_takeover_unconfirmed"
_MAX_EPOCH = (1 << 53) - 1  # Worker epochs are JavaScript safe integers.


class TakeoverAuthority(Protocol):
    """Independent protected admission and original stop custody.

    Both operations authenticate the current original recovery receipt and
    match its reservation (including shape), start ID, app, Worker version,
    original fence epoch and authorized actor. Check with authority time that
    the exact window is still open, armed and unfenced by external stop.
    Refuse stale or unknown state.
    """

    def confirm_current(self, window: EffectWindow, deployment: Deployment,
                        expected_epoch: int, actor: str, /) -> bool:
        """Read current custody, including after the attempt was consumed."""

    def claim_once(self, window: EffectWindow, deployment: Deployment,
                   expected_epoch: int, actor: str, /) -> bool:
        """Recheck custody and atomically consume the one attempt for this start.

        Persist consumption across runners before granting it. Never grant it
        again for that reservation/start ID, even after a lost claim or POST.
        """


def _confirm(operation, *args):
    try:
        confirmed = operation(*args)
    except Exception:
        confirmed = False
    require(confirmed is True, "pre_native_takeover_origin_unconfirmed")


def _check_window(window, clock):
    try:
        now = clock()
    except Exception:
        now = None
    window.require_open(now)


def _exchange(exchange, deployment, method, payload=None):
    """Return a bounded state or None for an ambiguous acknowledgement."""
    try:
        reply = exchange(deployment, method, CONTROL_PATH, payload)
    except Exception:
        return None
    if type(reply) is not OwnershipReply or type(reply.status) is not int:
        return None
    # A redirect or explicit client refusal cannot confirm a takeover.
    require(not (300 <= reply.status < 500 and reply.status != 408), _UNCONFIRMED)
    if reply.status != 200 or type(reply.body) is not bytes or len(reply.body) > CONTROL_REPLY_CAP:
        return None
    if method == "GET":
        require(type(reply.exact_takeover) is str and reply.exact_takeover == "1", _UNCONFIRMED)
    try:
        state = decode_json(reply.body.decode("utf-8"), max_depth=16)
    except (ValueError, RecursionError):
        return None
    if type(state) is not dict or "deploymentId" not in state:
        return None
    require(state["deploymentId"] == str(deployment.worker_version), _UNCONFIRMED)
    owner = state.get("owner")
    if (type(state.get("running")) is not bool or type(owner) is not dict
            or type(owner.get("epoch")) is not int or type(owner.get("oldEpoch")) is not int
            or type(owner.get("phase")) is not str or type(owner.get("actor")) is not str
            or "deploymentId" not in owner or "oldDeploymentId" not in owner):
        return None
    return state


def verify_native_takeover(deployment: Deployment, *, window: EffectWindow, expected_epoch: int, actor: str,
                           clock: Callable[[], int], authority: TakeoverAuthority,
                           exchange: OwnershipTransport) -> None:
    """Verify one takeover. The separately protected caller retains stop custody.

    The injected transport must use a fresh, authenticated, no-cache read on
    the exact private binding and check the durably armed window, origin and
    stop at the send/commit boundary. It must send the POST once. A direct
    acknowledgement may report a stopped Container because takeover does not
    start it. Reconciliation is read-only and requires it to be running.
    Readiness and stop/settlement remain the caller's responsibility.
    """
    require(type(deployment) is Deployment and type(deployment.target) is Target
            and all(type(value) is UUID and value.int for value in
                    (deployment.start_id, deployment.worker_version, deployment.container_id))
            and type(window) is EffectWindow and window.target == deployment.target
            and type(expected_epoch) is int and 1 <= expected_epoch < _MAX_EPOCH
            and type(actor) is str and re.fullmatch(r"[a-zA-Z0-9_.:@/-]{1,128}", actor),
            "pre_native_takeover_input_invalid")

    pins = (window, deployment, expected_epoch, actor)
    _check_window(window, clock)
    _confirm(authority.confirm_current, *pins)
    current = _exchange(exchange, deployment, "GET")
    require(current is not None and current["running"] is False, _UNCONFIRMED)
    prior = current["owner"]
    require(prior["phase"] == "fenced" and prior["deploymentId"] is None
            and prior["epoch"] == expected_epoch and prior["oldEpoch"] == expected_epoch - 1, _UNCONFIRMED)

    _check_window(window, clock)
    _confirm(authority.claim_once, *pins)
    _check_window(window, clock)
    version = str(deployment.worker_version)
    state = _exchange(exchange, deployment, "POST", {
        "action": "takeover-exact", "expectedDeploymentId": version,
        "expectedEpoch": expected_epoch, "actor": actor,
    })
    if state is None:
        _confirm(authority.confirm_current, *pins)
        state = _exchange(exchange, deployment, "GET")
        require(state is not None and state["running"] is True, _UNCONFIRMED)

    owner = state["owner"]
    require(owner["phase"] == "active" and owner["deploymentId"] == version
            and owner["epoch"] == expected_epoch + 1 and owner["oldEpoch"] == expected_epoch
            and owner["actor"] == actor and owner["oldDeploymentId"] is None, _UNCONFIRMED)
    _confirm(authority.confirm_current, *pins)
    _check_window(window, clock)
