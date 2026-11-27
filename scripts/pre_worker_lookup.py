"""Offline exact Worker lookup for a disposable PRE reservation."""

from dataclasses import dataclass
import re

from pre_target import Resource, Target, TargetError, require


ACCOUNT_ID = re.compile(r"[0-9a-f]{32}")
# Cloudflare api-schemas 9a5e4d44bd74d90fa9e4822142b8eb67e4ba3c70,
# Get Worker: HTTP 404 uses workers_ErrorWorkerNotFound.
WORKER_NOT_FOUND = 10007


@dataclass(frozen=True)
class WorkerLookupResponse:
    path: str
    status: int
    body: object


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
