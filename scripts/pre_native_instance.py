"""Read positive terminal observations from the Cloudflare native instance API."""

from dataclasses import dataclass
from datetime import datetime, timedelta
from http.client import HTTPException
import re
import time
from urllib.error import HTTPError
from urllib.request import Request

from pre_native_image_pin import NativeScope, _scope_valid
from pre_target import TargetError, require
from script_intake import decode_json, no_redirect_opener, read_with_overflow


@dataclass(frozen=True)
class TerminalInstance:
    scope: NativeScope
    stopped_at_ms: int
    observed_at_ms: int


def read_terminal_instance(scope: NativeScope, *, stop_requested_ms: int, token: str) -> TerminalInstance:
    """Read latest reported state with independent credentials and the admitted actor pin."""
    require(_scope_valid(scope) and type(stop_requested_ms) is int
            and scope.deployment.target.started_at * 1000 <= stop_requested_ms <= time.time_ns() // 1_000_000,
            "invalid_pre_instance_scope")
    require(type(token) is str and re.fullmatch(r"[A-Za-z0-9._~-]{1,512}", token),
            "invalid_pre_instance_credential")
    app_id = str(scope.deployment.container_id)
    url = (f"https://api.cloudflare.com/client/v4/accounts/{scope.account_id}"
           f"/containers/applications/{app_id}/instances/{scope.object_id}")
    request = Request(url, headers={"Authorization": f"Bearer {token}", "Cache-Control": "no-cache"})
    limit = 64 << 10
    reason = "pre_terminal_instance_unconfirmed"
    try:
        with no_redirect_opener(use_proxy=False).open(request, timeout=10) as reply:
            require(reply.status == 200, reason)
            raw = read_with_overflow(reply, limit)
            require(len(raw) <= limit, reason)
            envelope = decode_json(raw, max_depth=32)
        require(type(envelope) is dict and envelope.keys() == {"success", "errors", "messages", "result"}
                and envelope["success"] is True and envelope["errors"] == envelope["messages"] == [], reason)
        result = envelope["result"]
        require(type(result) is dict and result.get("id") == scope.object_id
                and result.get("application_id") == app_id, reason)
        status = result.get("status")
        require(type(status) is dict and status.get("state") == "stopped", reason)
        timestamp = status.get("updated_at")
        require(type(timestamp) is str and len(timestamp) <= 40, reason)
        updated = datetime.fromisoformat(timestamp)
        require(updated.utcoffset() == timedelta(0), reason)
        stopped_at = int(updated.timestamp() * 1000)
    except HTTPError as error:
        error.close()
        raise TargetError(reason) from None
    except (OSError, ValueError, OverflowError, RecursionError, HTTPException):
        raise TargetError(reason) from None
    observed_at = time.time_ns() // 1_000_000
    require(stop_requested_ms <= stopped_at <= observed_at, reason)
    return TerminalInstance(scope, stopped_at, observed_at)
