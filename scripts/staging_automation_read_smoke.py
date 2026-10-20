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

Registration identity contract (Discord application-command object,
https://docs.discord.com/developers/interactions/application-commands): `id`,
`application_id` and `version` are required snowflakes, `type` is optional
and defaults to 1 (slash command), `guild_id` is optional. Every read is
decoded with duplicate object keys refused at any depth. The list is admitted
for every targeted name before any resource is read: the name must appear on
exactly one entry, its id must be a canonical snowflake (ASCII decimal
1..2**64-1, no sign, padding or path/query characters) carried by no other
entry, and the entry must name the staging application, the fenced guild (when
it names one), slash type and a canonical version. One malformed or ambiguous
targeted entry fails that command and leaves every other admitted command
unread, so no resource URL is built from an unadmitted id; a missing name
fails only that command. The resource read must repeat the admitted id and
name and pass the same identity checks independently. The two GETs are not an
atomic snapshot: valid version drift between them is allowed and the receipt
records the resource (detail) version. Failure reasons are fixed vocabulary;
stdout and the receipt carry only admitted ids and versions, never remote
text. This is registration inspection only: no command handler is invoked.

Usage:
  DISCORD_STAGING_BOT_TOKEN=... DISCORD_STAGING_GUILD_ID=... \\
      python3 scripts/staging_automation_read_smoke.py [--evidence FILE]
"""

import argparse
from dataclasses import dataclass
import http.client
import json
import os
import re
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
# Discord snowflakes are unsigned 64-bit ids sent as decimal strings.
SNOWFLAKE_RE = re.compile(r"[1-9][0-9]{0,19}")
SNOWFLAKE_MAX = (1 << 64) - 1
SLASH_COMMAND_TYPE = 1
NOT_REGISTERED = "not registered in the staging guild"


class SmokeError(Exception):
    """A check's one-line failure reason (fixed vocabulary only)."""


class DuplicateKeyError(ValueError):
    """A JSON object repeats a key; the message never carries key/value data."""


def _reject_duplicate_keys(pairs):
    """object_pairs_hook refusing repeats instead of last-wins, at any depth."""
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise DuplicateKeyError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


def is_snowflake(value):
    """Canonical ASCII decimal 1..2**64-1 only: safe in a URL path and a receipt."""
    return (isinstance(value, str) and SNOWFLAKE_RE.fullmatch(value) is not None
            and int(value) <= SNOWFLAKE_MAX)


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


class _RefuseRedirect(urllib.request.HTTPRedirectHandler):
    """Authenticated reads never follow a redirect, even to the same origin.

    Raising here stops urllib before it builds the second request, so the bot
    token is never replayed; the Location and body are never read.
    """

    def http_error_301(self, req, fp, code, msg, headers):
        fp.close()
        raise SmokeError("refusing: authenticated read answered a redirect (not followed)")

    http_error_302 = http_error_303 = http_error_307 = http_error_308 = http_error_301


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
            with urllib.request.build_opener(_RefuseRedirect).open(
                    request, timeout=TIMEOUT_SECONDS) as response:
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
        value = json.loads(body, object_pairs_hook=_reject_duplicate_keys)
    except DuplicateKeyError:
        raise SmokeError("interactions endpoint answered 200 with duplicate JSON keys")
    except (ValueError, UnicodeDecodeError, RecursionError):
        raise SmokeError("interactions endpoint answered 200 with invalid JSON")
    return value


def check_identity(fetch_fn):
    """The token must belong to the staging application; anything else refuses."""
    me = get_json(fetch_fn, API + "/users/@me")
    if not isinstance(me, dict) or me.get("id") != STAGING_APPLICATION_ID:
        raise SmokeError("refusing: token is not the staging application")
    return me["id"]


def identity_fault(command, app_id, guild_id):
    """Fixed phrase for a command object that is not ours, or None.

    `type` defaults to the slash type when omitted and `guild_id` is optional
    on Discord's command object; a supplied value must still agree.
    """
    if command.get("application_id") != app_id:
        return "is not owned by the staging application"
    if "guild_id" in command and command["guild_id"] != guild_id:
        return "is not scoped to the staging guild"
    kind = command.get("type", SLASH_COMMAND_TYPE)
    if type(kind) is not int or kind != SLASH_COMMAND_TYPE:
        return "is not a slash command"
    if not is_snowflake(command.get("version")):
        return "carries no valid version"
    return None


def admit_entry(app_id, guild_id, entries, name):
    """List-stage admission: (command id, None) or (None, fixed failure reason)."""
    matches = [c for c in entries if isinstance(c, dict) and c.get("name") == name]
    if not matches:
        return None, NOT_REGISTERED
    if len(matches) > 1:
        return None, "registered more than once under this name"
    entry = matches[0]
    command_id = entry.get("id")
    if not is_snowflake(command_id):
        return None, "registered entry carries no valid command id"
    if sum(isinstance(c, dict) and c.get("id") == command_id for c in entries) > 1:
        return None, "registered command id is shared with another entry"
    fault = identity_fault(entry, app_id, guild_id)
    if fault is not None:
        return None, "registered entry " + fault
    return command_id, None


def read_command(fetch_fn, app_id, guild_id, name, command_id):
    """Read an admitted command's own resource and bind it to the list entry."""
    detail = get_json(
        fetch_fn,
        f"{API}/applications/{app_id}/guilds/{guild_id}/commands/{command_id}",
    )
    if (not isinstance(detail, dict) or detail.get("name") != name
            or detail.get("id") != command_id):
        return Result(name, False, "command resource disagrees with the list entry",
                      command_id=command_id)
    fault = identity_fault(detail, app_id, guild_id)
    if fault is not None:
        return Result(name, False, "command resource " + fault, command_id=command_id)
    # The detail version is admitted on its own; drift from the list is allowed.
    version = detail["version"]
    return Result(name, True, f"registered in the staging guild (version {version})",
                  command_id=command_id, version=version)


def check_command(fetch_fn, app_id, guild_id, entries, name):
    """Read one command's registration: list entry plus its own resource."""
    command_id, reason = admit_entry(app_id, guild_id, entries, name)
    if command_id is None:
        return Result(name, False, reason)
    return read_command(fetch_fn, app_id, guild_id, name, command_id)


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
        # Admit every target before the first resource read: one malformed or
        # ambiguous entry means the list itself is not trusted for any URL.
        targets = (CONTROL_COMMAND, *AUTOMATION_COMMANDS)
        admitted = [admit_entry(app_id, guild, entries, name) for name in targets]
        trusted = all(command_id is not None or reason == NOT_REGISTERED
                      for command_id, reason in admitted)
        results = []
        for name, (command_id, reason) in zip(targets, admitted):
            if command_id is None:
                results.append(Result(name, False, reason))
            elif not trusted:
                results.append(Result(name, False,
                                      "not read: another targeted entry failed list admission",
                                      command_id=command_id))
            else:
                results.append(read_command(fetch, app_id, guild, name, command_id))
    except SmokeError as error:
        return None, [Result("interactions", False, str(error))]
    return {"application_id": app_id, "guild_id": guild}, results


def evidence_shape(results):
    """Allowlisted receipt: names, verdicts, fixed reasons, admitted ids/versions only."""
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
