"""Offline exact Worker lookup for a disposable PRE reservation."""

from dataclasses import dataclass, field
import re
from uuid import UUID

from pre_target import NAMESPACE_ID, SHA, Resource, Target, TargetError, require


ACCOUNT_ID = re.compile(r"[0-9a-f]{32}")
# Cloudflare api-schemas 9a5e4d44bd74d90fa9e4822142b8eb67e4ba3c70,
# Get Worker: HTTP 404 uses workers_ErrorWorkerNotFound.
WORKER_NOT_FOUND = 10007


@dataclass(frozen=True)
class WorkerLookupResponse:
    path: str
    status: int
    body: object


@dataclass(frozen=True)
class WorkerVersionReply:
    target: Target
    path: str
    status: int
    body: object = field(repr=False)


def read_reserved_pre_qualification_namespace(target, *, account_id, version, source_sha, read):
    """Resolve the dedicated qualification class on an exact, source-stamped Worker version.

    The reader attests its Target and exact GET path from authenticated
    context. It rejects redirects and decodes a bounded, unambiguous provider
    response. This read does not establish which version serves requests.
    """
    require(type(target) is Target and type(account_id) is str and ACCOUNT_ID.fullmatch(account_id)
            and type(version) is UUID and version.int and type(source_sha) is str and SHA.fullmatch(source_sha),
            "invalid_pre_worker_version_pin")
    path = f"/accounts/{account_id}/workers/scripts/{target.worker_name}/versions/{version}"
    try:
        reply = read(path)
    except Exception:
        raise TargetError("pre_worker_version_read_failed") from None
    require(type(reply) is WorkerVersionReply and type(reply.target) is Target and reply.target == target
            and type(reply.path) is str and reply.path == path and type(reply.status) is int and reply.status == 200,
            "wrong_pre_worker_version_reply")
    body = reply.body
    require(type(body) is dict and body.keys() == {"success", "errors", "messages", "result"}
            and body["success"] is True and body["errors"] == body["messages"] == []
            and type(body["result"]) is dict, "unrecognized_pre_worker_version_reply")
    result = body["result"]
    annotations, resources = result.get("annotations"), result.get("resources")
    require(result.get("id") == str(version) and type(annotations) is dict
            and annotations.get("workers/message") == source_sha and type(resources) is dict,
            "wrong_pre_worker_version_detail")
    bindings = resources.get("bindings")
    require(type(bindings) is list and all(type(binding) is dict for binding in bindings),
            "invalid_pre_worker_version_bindings")
    bot = [binding for binding in bindings if binding.get("name") == "TWO_BOT"]
    require(len(bot) == 1 and bot[0].get("type") == "durable_object_namespace"
            and bot[0].get("class_name") == "PreQualificationContainer"
            and bot[0].get("script_name", target.worker_name) == target.worker_name,
            "pre_worker_qualification_class_unbound")
    namespace = bot[0].get("namespace_id")
    require(type(namespace) is str and NAMESPACE_ID.fullmatch(namespace), "invalid_pre_worker_qualification_namespace")
    return namespace


def read_reserved_worker(target, source_target, *, staging_account_id, read):
    """Return the exact name-keyed PRE Resource or None for proved absence.

    The caller pins staging_account_id independently and obtains source_target
    and read from the same authenticated staging read context. read makes one
    fresh GET on Cloudflare's v4 API origin with the supplied path. It rejects
    redirects and supplies the actual request path, HTTP status and a bounded
    JSON body decoded without duplicate keys or non-JSON values.

    TargetError means unknown. This read alone supplies neither Container
    inventory nor proof of startup settlement.
    """
    require(type(target) is Target and type(source_target) is Target and source_target == target,
            "wrong_pre_worker_target")
    require(type(staging_account_id) is str and ACCOUNT_ID.fullmatch(staging_account_id),
            "invalid_pre_worker_account")
    name = target.worker_name
    path = f"/accounts/{staging_account_id}/workers/workers/{name}"
    try:
        reply = read(path)
    except Exception:
        raise TargetError("pre_worker_read_failed") from None
    require(type(reply) is WorkerLookupResponse and type(reply.path) is str and reply.path == path
            and type(reply.status) is int, "wrong_pre_worker_reply")

    body = reply.body
    require(type(body) is dict and body.keys() == {"success", "errors", "messages", "result"}
            and type(body["messages"]) is list and not body["messages"], "unrecognized_pre_worker_reply")
    errors, result = body["errors"], body["result"]
    require(type(errors) is list, "unrecognized_pre_worker_reply")

    if reply.status == 200 and body["success"] is True and not errors:
        worker_id = result.get("id") if type(result) is dict else None
        require(type(result) is dict and type(result.get("name")) is str and result["name"] == name
                and type(worker_id) is str and worker_id.isprintable()
                and bool(worker_id.strip()), "wrong_pre_worker")
        # Inventory and cleanup identify the reserved script by name.
        return Resource(name, name)

    if reply.status == 404 and body["success"] is False and result is None and len(errors) == 1:
        error = errors[0]
        if (type(error) is dict and error.keys() == {"code", "message"}
                and type(error["code"]) is int and error["code"] == WORKER_NOT_FOUND
                and type(error["message"]) is str and error["message"].strip()):
            return None
    raise TargetError("pre_worker_lookup_unconfirmed")
