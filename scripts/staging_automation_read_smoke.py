#!/usr/bin/env python3
"""Read-only staging smoke for automation command read paths (stdlib only).

Reads the staging guild's registered application-command surface through
Discord's interactions (application-commands) REST endpoint and verifies the
three guild-scoped automation list commands answer their read paths:

  command-list    custom-command list surface
  schedule-list   scheduled-message list surface
  feed-list       feed-relay list surface (feeds are the polled automation
                  surface; see FeatureGates.feed_poll_seconds)

`rank` is a positive control: it is expected to be registered, so a missing
control proves the endpoint, credential or guild is wrong rather than the
automation surface being unpublished. Each surface is read twice: once in the
guild command list, once as its own command resource
(GET .../commands/{id}), which is the per-command read through the
interactions endpoint.

The smoke never writes, never touches production, and sends no credentials
anywhere except the Authorization header Discord itself requires. The only
network calls are GETs against discord.com/api/v10; the token comes only from
$DISCORD_STAGING_BOT_TOKEN (there is no flag for it, so it never lands in
shell history), and it is never printed, logged, or written to evidence.
A 401 refuses with a fixed message: rotation is an operator decision, and no
substitute credential is ever tried.

Body-cap refusal contract: the transport reads at most BODY_CAP+1 bytes so a
hidden suffix is detected. Any body longer than BODY_CAP (64 KiB) is refused
before JSON parsing with the fixed message "interactions endpoint answered
over the body cap"; exact-cap valid input passes, and the extra byte never
reaches a successful JSON path. Refusals never echo the body, headers, URL or
parse exceptions.

Guild fence (fail-closed, before any request): the run proceeds only when the
guild id is exactly the TWO Staging guild. The live guild, any other guild,
or a missing id refuses with exit 2 and no request is sent.

Usage:
  DISCORD_STAGING_BOT_TOKEN=... DISCORD_STAGING_GUILD_ID=... \\
      python3 scripts/staging_automation_read_smoke.py [--evidence FILE]
"""

import argparse
from dataclasses import dataclass
import http.client
import json
import os
import sys
import time
import urllib.error
import urllib.request

# Pinned identity guards. Sources: crates/core/src/backup/guild_config.rs
# (TWO_STAGING_GUILD_ID, STAGING_BOT_APPLICATION_ID) and
# crates/cutover/src/lib.rs (LIVE_GUILD_ID). No snowflake may be added here
# without updating those authorities first.
STAGING_GUILD_ID = "1545644954272137297"
LIVE_GUILD_ID = "326474832151838730"
STAGING_APPLICATION_ID = "1469137636663758888"

API = "https://discord.com/api/v10"
CONTROL_COMMAND = "rank"
AUTOMATION_COMMANDS = ("command-list", "schedule-list", "feed-list")
BODY_CAP = 64 << 10
TIMEOUT_SECONDS = 10
USER_AGENT = "two-bot-next-staging-automation-read-smoke/1.0 (read-only)"


class SmokeError(Exception):
    """A check's one-line failure reason (fixed vocabulary only)."""


@dataclass(frozen=True)
class Result:
    name: str
    ok: bool
    reason: str
    command_id: str | None = None
    version: str | None = None

    def line(self):
        return f"{'PASS' if self.ok else 'FAIL'} {self.name}: {self.reason}"


def guild_fence(guild_id):
    """Allowlist exactly the TWO Staging guild; refuse everything else unread."""
    if not guild_id:
        raise SmokeError("refusing: no staging guild id given ($DISCORD_STAGING_GUILD_ID)")
    if guild_id == LIVE_GUILD_ID:
        raise SmokeError("refusing: live guild id must never be smoked")
    if guild_id != STAGING_GUILD_ID:
        raise SmokeError("refusing: not the TWO Staging guild id")
    return guild_id


def require_token(token):
    if not token:
        raise SmokeError("refusing: no staging bot token given ($DISCORD_STAGING_BOT_TOKEN)")
    return token


def make_fetch(token):
    """Build the only network function; the token never leaves this closure."""

    def fetch(url):
        request = urllib.request.Request(
            url,
            method="GET",
            headers={"Authorization": "Bot " + token,
                     "User-Agent": USER_AGENT,
                     "Cache-Control": "no-cache"},
        )
        try:
            with urllib.request.build_opener().open(request, timeout=TIMEOUT_SECONDS) as response:
                return response.status, response.read(BODY_CAP + 1)
        except urllib.error.HTTPError as error:
            return error.code, error.read(BODY_CAP + 1)

    return fetch


def get_json(fetch_fn, url):
    """One authed GET returning parsed JSON; transport/auth failures are SmokeErrors."""
    try:
        status, body = fetch_fn(url)
    except (OSError, http.client.HTTPException) as error:
        raise SmokeError(f"interactions endpoint did not respond ({error.__class__.__name__})")
    if status == 401:
        raise SmokeError("credential refused (401); rotation is an operator decision")
    if status == 403:
        raise SmokeError("staging application lacks access (403)")
    if status == 429:
        raise SmokeError("rate limited (429); rerun later")
    if status != 200:
        raise SmokeError(f"interactions endpoint answered {status}, expected 200")
    if len(body) > BODY_CAP:
        raise SmokeError("interactions endpoint answered over the body cap")
    try:
        value = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        raise SmokeError("interactions endpoint answered 200 with invalid JSON")
    return value


def check_identity(fetch_fn):
    """The token must belong to the staging application; anything else refuses."""
    me = get_json(fetch_fn, API + "/users/@me")
    if not isinstance(me, dict) or me.get("id") != STAGING_APPLICATION_ID:
        raise SmokeError("refusing: token is not the staging application")
    return me["id"]


def check_command(fetch_fn, app_id, guild_id, entries, name):
    """Read one command's registration: list entry plus its own resource."""
    entry = next((c for c in entries if isinstance(c, dict) and c.get("name") == name), None)
    if entry is None:
        return Result(name, False, "not registered in the staging guild")
    command_id = entry.get("id")
    if not isinstance(command_id, str) or not command_id:
        return Result(name, False, "registered entry carries no command id")
    detail = get_json(
        fetch_fn,
        f"{API}/applications/{app_id}/guilds/{guild_id}/commands/{command_id}",
    )
    if not isinstance(detail, dict) or detail.get("name") != name:
        return Result(name, False, "command resource disagrees with the list entry",
                      command_id=command_id)
    if detail.get("guild_id") != guild_id:
        return Result(name, False, "command resource is not scoped to the staging guild",
                      command_id=command_id)
    version = detail.get("version")
    version_text = version if isinstance(version, str) and version else "unversioned"
    return Result(name, True,
                  f"registered in the staging guild (version {version_text})",
                  command_id=command_id,
                  version=version if isinstance(version, str) else None)


def run(guild_id, token, fetch_fn=None):
    """Fence first, then identity, then the control, then each automation surface."""
    try:
        guild = guild_fence(guild_id)
        require_token(token)
    except SmokeError as error:
        return None, [Result("guild-fence", False, str(error))]
    fetch = fetch_fn if fetch_fn is not None else make_fetch(token)
    try:
        app_id = check_identity(fetch)
        entries = get_json(fetch, f"{API}/applications/{app_id}/guilds/{guild}/commands")
        if not isinstance(entries, list):
            raise SmokeError("command list answered 200 without a list")
        results = [check_command(fetch, app_id, guild, entries, CONTROL_COMMAND)]
        results += [check_command(fetch, app_id, guild, entries, name)
                    for name in AUTOMATION_COMMANDS]
    except SmokeError as error:
        return None, [Result("interactions", False, str(error))]
    return {"application_id": app_id, "guild_id": guild}, results


def evidence_shape(results):
    """Allowlisted receipt: command names, verdicts and shape tokens only."""
    return [{"command": result.name, "verdict": "pass" if result.ok else "fail",
             "detail": result.reason, "command_id": result.command_id,
             "version": result.version} for result in results]


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--guild-id",
                        default=os.environ.get("DISCORD_STAGING_GUILD_ID"),
                        help="staging guild id (default: $DISCORD_STAGING_GUILD_ID)")
    parser.add_argument("--evidence", default="staging-automation-read-smoke-evidence.json",
                        help="receipt path written when the fetch phase completes")
    return parser.parse_args(argv)


def main(argv=None, fetch_fn=None):
    args = parse_args(argv)
    token = os.environ.get("DISCORD_STAGING_BOT_TOKEN", "")
    origin, results = run(args.guild_id, token, fetch_fn)
    for result in results:
        print(result.line())
    failed = sum(not result.ok for result in results)
    print(f"staging automation read smoke: {len(results) - failed}/{len(results)} checks passed")
    if origin is None:
        # Fence, credential or endpoint refusal: nothing to attest.
        return 2 if any(r.name == "guild-fence" for r in results) else 1
    receipt = {"application_id": origin["application_id"],
               "guild_id": origin["guild_id"],
               "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "commands": evidence_shape(results),
               "result": "pass" if not failed else "fail"}
    try:
        with open(args.evidence, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=2)
            handle.write("\n")
    except OSError as error:
        print(f"staging automation read smoke failed: evidence not written ({error.__class__.__name__})")
        return 1
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
