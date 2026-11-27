"""Dormant ownership bootstrap for the archived isolated PRE Worker."""

from dataclasses import dataclass, field
import re
from typing import Protocol
from uuid import UUID

from pre_start_stop import Deployment
from pre_target import Target, TargetError, require
from script_intake import decode_json


CONTROL_PATH = "/internal/ownership"
CONTROL_REPLY_CAP = 4096
UNCONFIRMED = "pre_ownership_unconfirmed"


@dataclass(frozen=True)
class OwnershipReply:
    status: int
    body: bytes = field(repr=False)
    exact_takeover: str | None = None  # Single x-two-bot-exact-takeover response header.


class OwnershipTransport(Protocol):
    """Send once over the separately authenticated, pinned private binding.

    The archived outer Worker accepts a control bearer and stamps its own
    deployment header on the forwarded Durable Object request.
    Check the reservation, start ID, deployment and armed stop at each effect
    boundary. Bound time and reads to a deadline and CONTROL_REPLY_CAP plus
    one byte for overflow detection. Refuse redirects and ambiguous headers.
    """

    def __call__(self, deployment: Deployment, method: str, path: str,
                 payload: dict[str, str | int] | None, /) -> OwnershipReply: ...


def _exchange(exchange, deployment, method, payload):
    try:
        reply = exchange(deployment, method, CONTROL_PATH, payload)
        require(type(reply) is OwnershipReply and type(reply.status) is int and reply.status == 200
                and type(reply.body) is bytes and len(reply.body) <= CONTROL_REPLY_CAP, UNCONFIRMED)
        state = decode_json(reply.body.decode("utf-8"), max_depth=16)
        require(type(state) is dict and state.get("deploymentId") == str(deployment.worker_version)
                and state.get("running") is False and "owner" in state, UNCONFIRMED)
        if method == "GET":
            require(type(reply.exact_takeover) is str and reply.exact_takeover == "1", UNCONFIRMED)
        return state["owner"]
    except Exception:
        pass
    raise TargetError(UNCONFIRMED) from None


def _confirm_owner(owner, *, phase, epoch, deployment_id, actor):
    require(type(owner) is dict and owner.get("phase") == phase
            and type(owner.get("epoch")) is int and owner["epoch"] == epoch
            and type(owner.get("oldEpoch")) is int and owner["oldEpoch"] == epoch - 1
            and owner.get("actor") == actor
            and "deploymentId" in owner and owner["deploymentId"] == deployment_id
            and "oldDeploymentId" in owner and owner["oldDeploymentId"] is None, UNCONFIRMED)


def bootstrap_fresh_ownership(deployment: Deployment, *, actor: str, exchange: OwnershipTransport) -> None:
    """Run inside create_private after arm and before returning the verified deployment.

    A refused or lost response ends the attempt. The caller retains the
    reservation for the independently authorized ProtectedPre.stop() path.
    """
    require(type(deployment) is Deployment and type(deployment.target) is Target
            and all(type(value) is UUID and value.int for value in
                    (deployment.start_id, deployment.worker_version, deployment.container_id))
            and type(actor) is str and len(actor) <= 128 and re.fullmatch(r"[a-zA-Z0-9_.:@/-]+", actor),
            "pre_ownership_input_invalid")
    require(_exchange(exchange, deployment, "GET", None) is None, UNCONFIRMED)

    # The archived Worker requires a persisted ownerless fence for exact takeover.
    fenced = _exchange(exchange, deployment, "POST", {"action": "fence", "expectedEpoch": 0, "actor": actor})
    _confirm_owner(fenced, phase="fenced", epoch=1, deployment_id=None, actor=actor)

    version = str(deployment.worker_version)
    active = _exchange(exchange, deployment, "POST", {
        "action": "takeover-exact", "expectedEpoch": fenced["epoch"], "actor": actor, "expectedDeploymentId": version,
    })
    _confirm_owner(active, phase="active", epoch=2, deployment_id=version, actor=actor)
