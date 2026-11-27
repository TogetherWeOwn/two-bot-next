"""Dormant exact Container app lookup for a disposable PRE reservation."""

from dataclasses import dataclass, field
import re

from pre_inventory import RESOURCE_NAME
from pre_registry_cleanup import RegistryReservation
from pre_target import APP_ID, NAMESPACE_ID, Resource, Target, TargetError, require


ACCOUNT_ID = re.compile(r"[0-9a-f]{32}")
# The Containers API's generic error schema uses this routing error as its example.
NO_ROUTE_FOR_URI = 7003


@dataclass(frozen=True)
class ContainerAppScope:
    target: Target
    account_id: str
    app_id: str

    def __post_init__(self):
        require(type(self.target) is Target, "wrong_pre_container_target")
        require(type(self.account_id) is str and ACCOUNT_ID.fullmatch(self.account_id),
                "invalid_pre_container_account")
        require(type(self.app_id) is str and APP_ID.fullmatch(self.app_id), "invalid_pre_app_pin")


@dataclass(frozen=True)
class ContainerAppReply:
    scope: ContainerAppScope
    path: str
    status: int
    body: object = field(repr=False)
    application_not_found: bool = False


def read_reserved_container_app(scope, *, read):
    """Return the pinned Resource, or None for authoritative app absence.

    The caller pins scope independently from the reservation and provisioned app.
    The injected reader makes one fresh GET on the Cloudflare v4 API origin with
    no redirects. It supplies its authenticated scope, actual path, HTTP status
    and a bounded, strictly decoded JSON body. Scope must come from its read
    authority, including the full reservation, rather than the request or body.

    Get Application documents an application-not-found 404 with a generic v4
    error schema. The reader sets application_not_found only when it can
    establish that endpoint's documented outcome, including account read access
    and that the error came from the application handler. It establishes that
    provenance independently of the transport status and generic error JSON.

    TargetError means unknown. Cleanup requires separate instance counts and
    startup settlement evidence.
    """
    result = _read_reserved_container_app(scope, read=read)
    return None if result is None else Resource(result["name"], result["id"])


def confirm_reserved_container_configuration(scope, *, namespace_id, image, read) -> None:
    """Check the configured namespace and image of one reserved scheduler-backed app.

    Pin namespace_id from the exact Worker version's Container binding and image
    from the verified immutable registry publication for the same Target.
    Reuse the authenticated reader contract of read_reserved_container_app.
    A match establishes app configuration only. Running image, effective writer
    and process launch authorization require independent evidence.
    """
    require(type(scope) is ContainerAppScope, "invalid_pre_container_scope")
    require(type(namespace_id) is str and RESOURCE_NAME.fullmatch(namespace_id),
            "invalid_pre_container_namespace")
    require(type(image) is RegistryReservation and image.target == scope.target,
            "wrong_pre_container_image_scope")
    result = _read_configured_namespace(scope, namespace_id, read=read)
    config = result.get("configuration")
    require(type(config) is dict and type(config.get("image")) is str
            and config["image"] == image.reference, "pre_container_image_mismatch")


def confirm_reserved_durable_object_app(scope, *, namespace_id, read) -> None:
    """Confirm the namespace and native scheduler of one reserved Container app.

    This scheduler chooses the image in the Durable Object's start call. Get
    Application supplies no image pin for this policy.
    """
    require(type(namespace_id) is str and NAMESPACE_ID.fullmatch(namespace_id),
            "invalid_pre_container_namespace")
    result = _read_configured_namespace(scope, namespace_id, read=read)
    require(result.get("scheduling_policy") == "durable_object", "wrong_pre_container_scheduling_policy")


def _read_configured_namespace(scope, namespace_id, *, read):
    result = _read_reserved_container_app(scope, read=read)
    require(result is not None, "pre_container_configuration_unconfirmed")
    namespace = result.get("durable_objects")
    require(type(namespace) is dict and type(namespace.get("namespace_id")) is str
            and namespace["namespace_id"] == namespace_id, "pre_container_namespace_mismatch")
    return result


def _read_reserved_container_app(scope, *, read):
    require(type(scope) is ContainerAppScope, "invalid_pre_container_scope")
    path = f"/accounts/{scope.account_id}/containers/applications/{scope.app_id}"
    try:
        reply = read(path)
    except Exception:
        raise TargetError("pre_container_read_failed") from None
    require(type(reply) is ContainerAppReply and type(reply.scope) is ContainerAppScope
            and reply.scope == scope and type(reply.path) is str and reply.path == path
            and type(reply.status) is int and type(reply.application_not_found) is bool,
            "wrong_pre_container_reply")

    body = reply.body
    require(type(body) is dict and body.keys() == {"success", "errors", "messages", "result"}
            and type(body["errors"]) is list and type(body["messages"]) is list and not body["messages"],
            "unrecognized_pre_container_reply")
    result, errors = body["result"], body["errors"]
    if reply.status == 200 and body["success"] is True and not errors and not reply.application_not_found:
        name = scope.target.container_name
        require(type(result) is dict and all(type(result.get(key)) is str and result[key] == expected
                for key, expected in (("account_id", scope.account_id), ("id", scope.app_id), ("name", name))),
                "wrong_pre_app_detail")
        return result

    require(reply.status == 404 and reply.application_not_found and body["success"] is False
            and result is None and len(errors) == 1, "pre_container_lookup_unconfirmed")
    error = errors[0]
    require(type(error) is dict and error.keys() == {"code", "message"}
            and type(error["code"]) is int and error["code"] >= 1000
            and error["code"] != NO_ROUTE_FOR_URI and type(error["message"]) is str
            and error["message"].strip(), "pre_container_lookup_unconfirmed")
    return None
