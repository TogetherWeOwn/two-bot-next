#!/usr/bin/env python3
"""Authenticated staging MCP prove-out (stdlib only, read-only).

Runs in CI as the `staging` environment holder of STAGING_MCP_TOKEN, so no
agent ever handles the bearer. The token arrives only via $STAGING_MCP_TOKEN
(GitHub masks it as ***); it is sent solely as an `Authorization: Bearer`
header and never appears in stdout, files, artifacts or error text. Every
logged command shows the bearer as ***.

Probes against $STAGING_MCP_URL (default the staging Worker /mcp) with
`User-Agent: two-bot-next-staging-rollout/1.0`:
  * initialize -> result must carry a non-empty protocolVersion.
  * tools/list -> result.tools must be exactly the five read-only tools.
  * tools/call each of the five -> bodies are recorded. While staging is
    fenced (/readyz 503 ownership_fenced/not_owner) the container-backed trio
    (get_status, get_gateway, get_backfill_progress) may return JSON-RPC
    -32000 "container request failed"; that is expected, not a failure.
    get_config (redacted Worker snapshot), get_audit (implemented:false stub),
    initialize, tools/list and unknown-tool -32601 are Worker-side and must
    succeed regardless.
  * tools/call unknown tool -> JSON-RPC -32601.
  * wrong bearer (fixed dummy, never the real token) and no bearer ->
    HTTP 401 with WWW-Authenticate: Bearer.

Output: redacted commands plus full response bodies on stdout (responses
carry no credential), plus a sanitized JSON artifact with the same bodies
and no bearer. Bodies over 64 KiB are refused before parsing and recorded
as probe failures with the fixed cap verdict. Exit 0 proves the
Worker-side slice; exit 1 is a probe failure; exit 2 is a pre-flight
refusal (nothing was sent).

Usage (the workflow supplies the environment):
  python3 scripts/staging_mcp_proveout.py --output staging-mcp-proveout-<run_id>.json
"""

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone
from urllib.parse import urlsplit

UA = "two-bot-next-staging-rollout/1.0"
DEFAULT_URL = "https://two-bot-next-staging.5150.workers.dev/mcp"
TOKEN_ENV = "STAGING_MCP_TOKEN"
URL_ENV = "STAGING_MCP_URL"
MIN_TOKEN_LENGTH = 32
STAGING_HOST = re.compile(r"two-bot-next-staging\.[a-z0-9-]+\.workers\.dev")
# Response bound (64 KiB, matching MCP_MAX_BODY_BYTES in wrangler/src/mcp.ts
# and MAX_BODY_BYTES in the sibling staging readers): every response read
# takes at most MAX_BODY_BYTES+1 bytes so a hidden suffix is detected. Any
# longer body raises BodyTooLarge before decoding; the oversize bytes never
# reach parsing, logs or the artifact. Refusals never echo the body or the
# bearer.
MAX_BODY_BYTES = 64 * 1024
REQUEST_TIMEOUT_SECONDS = 25
SCHEMA_VERSION = 1

EXPECTED_TOOLS = [
    "get_status",
    "get_gateway",
    "get_backfill_progress",
    "get_config",
    "get_audit",
]
CONTAINER_BACKED = {"get_status", "get_gateway", "get_backfill_progress"}
WRONG_BEARER = "wrong-bearer-probe-0000"


class BodyTooLarge(ValueError):
    """A response body exceeds MAX_BODY_BYTES (fixed vocabulary only)."""


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """Refuse redirects so the bearer never leaves STAGING_MCP_URL.

    The default opener follows 301/302/303 by re-issuing a GET to the
    Location URL with every non-content header copied, including
    Authorization, without checking host or scheme. Returning None makes
    the opener surface the 3xx as an HTTPError, which do_post records as
    a probe failure instead of forwarding the bearer.
    """

    def redirect_request(self, *args, **kwargs):
        return None


def fail_refusal(message):
    print(f"REFUSED: {message}", flush=True)
    return 2


def valid_staging_mcp_url(url):
    """Only the exact staging https /mcp URL; refuse userinfo, port, query and fragment."""
    if not isinstance(url, str) or not url:
        return False
    try:
        parts = urlsplit(url)
    except ValueError:
        return False
    try:
        port = parts.port
    except ValueError:
        return False
    return (parts.scheme == "https" and port is None
            and parts.username is None and parts.password is None
            and bool(STAGING_HOST.fullmatch(parts.hostname or ""))
            and parts.path == "/mcp" and not parts.query and not parts.fragment)


def redacted_request_line(rpc_method, rpc_id, extra=""):
    base = f'POST /mcp {{"jsonrpc":"2.0","id":{rpc_id},"method":"{rpc_method}"'
    if extra:
        base += f",{extra}"
    base += '} Authorization: Bearer ***'
    return base


def do_post(url, payload, token_or_none, timeout=REQUEST_TIMEOUT_SECONDS):
    """POST one JSON-RPC payload. Returns (http_status, headers_dict, body_text).

    Network errors return (None, {}, "<error-class>") with no secret text.
    The bearer is never included in any returned string. Redirects are
    refused (NoRedirect) so the bearer never leaves the target origin;
    a 3xx surfaces as its HTTP status and is recorded as a failure.
    Both read paths take at most MAX_BODY_BYTES+1 bytes; a longer body
    raises BodyTooLarge before decoding/parsing, never echoing body bytes.
    """
    data = json.dumps(payload).encode("utf-8")
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json",
        "User-Agent": UA,
    }
    if token_or_none is not None:
        headers["Authorization"] = f"Bearer {token_or_none}"
    req = urllib.request.Request(url, data=data, headers=headers, method="POST")
    try:
        opener = urllib.request.build_opener(NoRedirect)
        with opener.open(req, timeout=timeout) as resp:
            status = resp.status
            resp_headers = {k.lower(): v for k, v in resp.headers.items()}
            raw = resp.read(MAX_BODY_BYTES + 1)
            if len(raw) > MAX_BODY_BYTES:
                raise BodyTooLarge("response body over the size cap")
            body = raw.decode("utf-8", errors="replace")
            return status, resp_headers, body
    except urllib.error.HTTPError as e:
        try:
            raw = e.read(MAX_BODY_BYTES + 1)
            if len(raw) > MAX_BODY_BYTES:
                raise BodyTooLarge("response body over the size cap")
            body = raw.decode("utf-8", errors="replace")
        except BodyTooLarge:
            raise
        except Exception:
            body = ""
        resp_headers = {k.lower(): v for k, v in (e.headers.items() if e.headers else [])}
        return e.code, resp_headers, body
    except BodyTooLarge:
        raise
    except Exception:
        return None, {}, "network-error"


def parse_json_or_text(body):
    try:
        return json.loads(body)
    except Exception:
        return {"_text": body[:MAX_BODY_BYTES]}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--output", required=True)
    args = ap.parse_args()

    token = os.environ.get(TOKEN_ENV, "")
    if not token or len(token) < MIN_TOKEN_LENGTH:
        return fail_refusal(f"${TOKEN_ENV} missing or shorter than {MIN_TOKEN_LENGTH}; nothing sent")
    url = os.environ.get(URL_ENV, DEFAULT_URL).strip() or DEFAULT_URL
    if not valid_staging_mcp_url(url):
        return fail_refusal(
            "staging MCP URL must be the exact staging https URL "
            "two-bot-next-staging.<sub>.workers.dev/mcp; nothing sent")

    probes = []
    failures = []

    def record(name, redacted_cmd, http_status, headers, body_text):
        body_json = parse_json_or_text(body_text)
        # Keep only the safe response headers; never store request headers.
        safe_headers = {}
        for key in ("content-type", "www-authenticate", "allow", "retry-after"):
            if key in headers:
                safe_headers[key] = headers[key]
        probes.append(
            {
                "name": name,
                "redacted_command": redacted_cmd,
                "http_status": http_status,
                "response_headers": safe_headers,
                "body": body_json,
            }
        )
        print(f"$ {redacted_cmd}", flush=True)
        print(f"HTTP {http_status}", flush=True)
        for key, value in safe_headers.items():
            print(f"{key}: {value}", flush=True)
        print(json.dumps(body_json, indent=2, sort_keys=True), flush=True)
        print(flush=True)

    def fetch(name, redacted_cmd, payload, token_or_none):
        """POST one probe, mapping BodyTooLarge to a bounded cap failure.

        Returns (status, headers, body) on success; on refusal records a
        bounded probe (no body bytes kept) with the fixed cap verdict and
        returns (None, {}, None) so callers skip shape validation.
        """
        try:
            status, headers, body = do_post(url, payload, token_or_none)
        except BodyTooLarge:
            record(name, redacted_cmd, None, {}, "")
            failures.append(f"{name}: answered over the body cap")
            return None, {}, None
        record(name, redacted_cmd, status, headers, body)
        return status, headers, body

    rpc_id = 1

    # 1. initialize (authenticated).
    payload = {
        "jsonrpc": "2.0",
        "id": rpc_id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "staging-mcp-proveout", "version": "1.0"},
        },
    }
    status, headers, body = fetch("initialize", redacted_request_line("initialize", rpc_id, '"params":{...}'), payload, token)
    if body is not None:
        parsed = parse_json_or_text(body)
        result = parsed.get("result") if isinstance(parsed, dict) else None
        if status != 200 or not isinstance(result, dict) or not result.get("protocolVersion"):
            failures.append("initialize: expected HTTP 200 with result.protocolVersion")
    rpc_id += 1

    # 2. tools/list (authenticated).
    payload = {"jsonrpc": "2.0", "id": rpc_id, "method": "tools/list", "params": {}}
    status, headers, body = fetch("tools/list", redacted_request_line("tools/list", rpc_id), payload, token)
    if body is not None:
        parsed = parse_json_or_text(body)
        tools = (parsed.get("result", {}) or {}).get("tools") if isinstance(parsed, dict) else None
        names = sorted(t.get("name") for t in tools) if isinstance(tools, list) else None
        if status != 200 or names != sorted(EXPECTED_TOOLS):
            failures.append(f"tools/list: expected exactly {','.join(sorted(EXPECTED_TOOLS))}, got {names}")
    rpc_id += 1

    # 3-7. tools/call each of the five (authenticated, bodies recorded).
    for tool in EXPECTED_TOOLS:
        payload = {
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": {}},
        }
        status, headers, body = fetch(
            f"tools/call {tool}",
            redacted_request_line("tools/call", rpc_id, f'"name":"{tool}"'),
            payload,
            token,
        )
        if body is None:
            rpc_id += 1
            continue
        parsed = parse_json_or_text(body)
        if status != 200 or not isinstance(parsed, dict):
            failures.append(f"tools/call {tool}: expected HTTP 200 JSON-RPC envelope")
        elif "error" in parsed and parsed["error"] is not None:
            err = parsed["error"] or {}
            code = err.get("code")
            if tool in CONTAINER_BACKED and code == -32000:
                print(f"NOTE: tools/call {tool} returned -32000 (fenced container); expected while staging is fenced.", flush=True)
            elif tool == "get_config":
                failures.append(f"tools/call get_config: must succeed Worker-side, got error {code}")
            elif tool == "get_audit":
                failures.append(f"tools/call get_audit: must succeed Worker-side, got error {code}")
            else:
                failures.append(f"tools/call {tool}: unexpected JSON-RPC error code {code}")
        else:
            result = parsed.get("result") if isinstance(parsed, dict) else None
            if not isinstance(result, dict) or "content" not in result:
                failures.append(f"tools/call {tool}: expected result.content envelope")
            elif tool == "get_audit":
                structured = result.get("structuredContent") or {}
                if structured.get("implemented") is not False:
                    failures.append("tools/call get_audit: expected structuredContent.implemented=false stub")
        rpc_id += 1

    # 8. tools/call unknown tool (authenticated) -> -32601.
    payload = {
        "jsonrpc": "2.0",
        "id": rpc_id,
        "method": "tools/call",
        "params": {"name": "no_such_tool", "arguments": {}},
    }
    status, headers, body = fetch(
        "tools/call unknown",
        redacted_request_line("tools/call", rpc_id, '"name":"no_such_tool"'),
        payload,
        token,
    )
    if body is not None:
        parsed = parse_json_or_text(body)
        err = (parsed.get("error") or {}) if isinstance(parsed, dict) else {}
        if status != 200 or err.get("code") != -32601:
            failures.append(f"tools/call unknown: expected JSON-RPC -32601, got HTTP {status} {err}")
    rpc_id += 1

    # 9. Wrong bearer (fixed dummy, never the real token) -> 401 Bearer.
    payload = {"jsonrpc": "2.0", "id": rpc_id, "method": "tools/list", "params": {}}
    status, headers, body = fetch(
        "wrong bearer",
        'POST /mcp {"jsonrpc":"2.0","method":"tools/list"} Authorization: Bearer ***wrong***',
        payload,
        WRONG_BEARER,
    )
    if body is not None:
        www = (headers.get("www-authenticate") or "")
        if status != 401 or "bearer" not in www.lower():
            failures.append(f"wrong bearer: expected HTTP 401 WWW-Authenticate: Bearer, got HTTP {status}")
    rpc_id += 1

    # 10. No bearer -> 401 Bearer (cheap re-confirm of the Operator check).
    status, headers, body = fetch(
        "no bearer",
        'POST /mcp {"jsonrpc":"2.0","method":"tools/list"} Authorization: (absent)',
        payload,
        None,
    )
    if body is not None:
        www = (headers.get("www-authenticate") or "")
        if status != 401 or "bearer" not in www.lower():
            failures.append(f"no bearer: expected HTTP 401 WWW-Authenticate: Bearer, got HTTP {status}")

    artifact = {
        "schema_version": SCHEMA_VERSION,
        "proved_at": datetime.now(timezone.utc).isoformat(),
        "mcp_url": url,
        "user_agent": UA,
        "expected_tools": sorted(EXPECTED_TOOLS),
        "note": "Bodies carry no credential. Bearer never logged, stored, or uploaded.",
        "probes": probes,
        "failures": failures,
    }
    with open(args.output, "w", encoding="utf-8") as f:
        json.dump(artifact, f, indent=2, sort_keys=True)
        f.write("\n")

    if failures:
        print("FAILURES:", flush=True)
        for item in failures:
            print(f"- {item}", flush=True)
        return 1
    print("OK: Worker-side MCP slice proved out (container trio may show expected -32000 while fenced).", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
