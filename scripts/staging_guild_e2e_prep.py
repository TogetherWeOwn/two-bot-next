#!/usr/bin/env python3
"""Mock-only staging-guild E2E prep skeleton (stdlib only).

Drives the guild interactions-endpoint shape against LOCAL fixtures through
an injected fetch function: identity (`GET /users/@me`), the guild command
list, then one per-command resource read per surface (the per-command read
through the interactions endpoint). There is no network code and no token
handling in this module: the only fetch it ever calls is the one the caller
injects (`--mock` builds it from the fixtures below), so the skeleton run is
mock-only by construction.

Guild fence (fail-closed, before anything else): the run proceeds only when
the guild id is exactly the TWO Staging guild. A live-guild id aborts before
any request is built or sent; any other or missing id refuses the same way.

Surfaces (each read twice: once in the guild command list, once as its own
command resource):

  rank          positive control: expected registered, so a missing control
                proves the fixture or guild is wrong rather than the surface
                being unpublished.
  leaderboard   second core publish-set surface
                (`core_commands` in crates/core/src/commands.rs): proves the
                list-plus-resource shape a second time.

Registration identity contract: the list is admitted for every targeted
name before any resource is read: the name must appear on exactly one
entry, its id must be a canonical snowflake (ASCII decimal 1..2**64-1,
no sign, padding or path/query characters) carried by no other entry,
and the entry must name the fenced guild (when it names one) and a
canonical version. One malformed or ambiguous targeted entry fails that
command and leaves every other admitted command unread, so no resource
URL is built from an unadmitted id; a missing name fails only that
command. The resource read must repeat the admitted id and name and
pass the same identity checks independently. Valid version drift
between the two reads is allowed and the receipt records the resource
(detail) version. Failure reasons are fixed vocabulary; stdout and the
receipt carry only admitted ids and versions, never remote text.

The live Discord transport (authed GETs against discord.com/api/v10) arrives
with the full staging suite; this skeleton never reads a token and sends no
credentials anywhere.

Usage (mock fixtures only, no network):
  python3 scripts/staging_guild_e2e_prep.py --mock [--evidence FILE]
"""

import argparse
from dataclasses import dataclass
import json
import os
import re
import sys
import time

# Pinned identity guards. Sources: crates/core/src/backup/guild_config.rs
# (TWO_STAGING_GUILD_ID, STAGING_BOT_APPLICATION_ID) and
# crates/cutover/src/lib.rs (LIVE_GUILD_ID). No snowflake may be added here
# without updating those authorities first.
STAGING_GUILD_ID = "1545644954272137297"
LIVE_GUILD_ID = "326474832151838730"
STAGING_APPLICATION_ID = "1469137636663758888"

API = "https://discord.com/api/v10"
CONTROL_COMMAND = "rank"
PREP_COMMANDS = ("leaderboard",)
MOCK_TRANSPORT = "mock-local-fixtures"
BODY_CAP = 64 << 10


class PrepError(Exception):
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
        raise PrepError("refusing: no staging guild id given ($DISCORD_STAGING_GUILD_ID)")
    if guild_id == LIVE_GUILD_ID:
        raise PrepError("refusing: live guild id must never be touched by E2E prep")
    if guild_id != STAGING_GUILD_ID:
        raise PrepError("refusing: not the TWO Staging guild id")
    return guild_id


def fixture_commands():
    """Local publish-set fixture: the endpoint shape, not a live registry."""
    return [
        {"id": "1549621908675366953", "name": "rank", "type": 1,
         "guild_id": STAGING_GUILD_ID, "version": "1000000000000000001"},
        {"id": "2000000000000000005", "name": "leaderboard", "type": 1,
         "guild_id": STAGING_GUILD_ID, "version": "1000000000000000005"},
    ]


def fixture_fetch(commands=None):
    """Mock transport over local fixtures: URL in, (status, body) out."""
    entries = commands if commands is not None else fixture_commands()
    app_url = f"{API}/applications/{STAGING_APPLICATION_ID}/guilds/{STAGING_GUILD_ID}/commands"
    routes = {
        f"{API}/users/@me": (200, json.dumps(
            {"id": STAGING_APPLICATION_ID, "username": "fixture"}).encode()),
        app_url: (200, json.dumps(entries).encode()),
    }
    for command in entries:
        routes[f"{app_url}/{command['id']}"] = (200, json.dumps(command).encode())

    def fetch(url):
        if url not in routes:
            raise PrepError(f"interactions endpoint has no fixture route for {url}")
        return routes[url]

    return fetch


def get_json(fetch_fn, url):
    """One mock GET returning parsed JSON; shape failures are PrepErrors."""
    try:
        status, body = fetch_fn(url)
    except PrepError:
        raise
    except Exception as error:
        raise PrepError(f"interactions endpoint did not respond ({error.__class__.__name__})")
    if status != 200:
        raise PrepError(f"interactions endpoint answered {status}, expected 200")
    if len(body) > BODY_CAP:
        raise PrepError("interactions endpoint answered over the body cap")
    try:
        return json.loads(body)
    except (ValueError, UnicodeDecodeError, RecursionError):
        raise PrepError("interactions endpoint answered 200 with invalid JSON")


def check_identity(fetch_fn):
    """The transport must answer as the staging application; anything else refuses."""
    me = get_json(fetch_fn, API + "/users/@me")
    if not isinstance(me, dict) or me.get("id") != STAGING_APPLICATION_ID:
        raise PrepError("refusing: transport is not the staging application")
    return me["id"]


NOT_REGISTERED = "not registered in the staging guild"

# Discord snowflakes are unsigned 64-bit ids sent as decimal strings.
SNOWFLAKE_RE = re.compile(r"[1-9][0-9]{0,19}")
SNOWFLAKE_MAX = (1 << 64) - 1


def is_snowflake(value):
    """Canonical ASCII decimal 1..2**64-1 only: safe in a URL path and a receipt."""
    return (isinstance(value, str) and SNOWFLAKE_RE.fullmatch(value) is not None
            and int(value) <= SNOWFLAKE_MAX)


def identity_fault(command, guild_id):
    """Fixed phrase for a command object with no usable identity, or None.

    E1 scope is the registration binding (guild scope when named, canonical
    version); the staging-application and slash-type pins stay on the
    automation copy.
    """
    if "guild_id" in command and command["guild_id"] != guild_id:
        return "is not scoped to the staging guild"
    if not is_snowflake(command.get("version")):
        return "carries no valid version"
    return None


def admit_entry(guild_id, entries, name):
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
    fault = identity_fault(entry, guild_id)
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
    fault = identity_fault(detail, guild_id)
    if fault is not None:
        return Result(name, False, "command resource " + fault, command_id=command_id)
    # The detail version is admitted on its own; drift from the list is allowed.
    version = detail["version"]
    return Result(name, True,
                  f"registered in the staging guild (version {version})",
                  command_id=command_id, version=version)


def check_command(fetch_fn, app_id, guild_id, entries, name):
    """Read one command's registration: list entry plus its own resource."""
    command_id, reason = admit_entry(guild_id, entries, name)
    if command_id is None:
        return Result(name, False, reason)
    return read_command(fetch_fn, app_id, guild_id, name, command_id)


def run(guild_id, fetch_fn):
    """Fence first, then identity, then the control, then each prep surface."""
    try:
        guild = guild_fence(guild_id)
    except PrepError as error:
        return None, [Result("guild-fence", False, str(error))]
    if fetch_fn is None:
        return None, [Result("transport", False,
                             "no transport given (pass --mock for local fixtures)")]
    try:
        app_id = check_identity(fetch_fn)
        entries = get_json(fetch_fn, f"{API}/applications/{app_id}/guilds/{guild}/commands")
        if not isinstance(entries, list):
            raise PrepError("command list answered 200 without a list")
        # Admit every target before the first resource read: one malformed or
        # ambiguous entry means the list itself is not trusted for any URL.
        targets = (CONTROL_COMMAND, *PREP_COMMANDS)
        admitted = [admit_entry(guild, entries, name) for name in targets]
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
                results.append(read_command(fetch_fn, app_id, guild, name, command_id))
    except PrepError as error:
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
    parser.add_argument("--mock", action="store_true",
                        help="drive the endpoint shape against local fixtures (no network)")
    parser.add_argument("--evidence", default="staging-guild-e2e-prep-evidence.json",
                        help="receipt path written when the fetch phase completes")
    return parser.parse_args(argv)


def main(argv=None, fetch_fn=None):
    args = parse_args(argv)
    # An injected fetch always wins: tests drive every refusal through
    # doubles, and the CLI only builds fixtures when --mock is passed.
    fetch = fetch_fn if fetch_fn is not None else (fixture_fetch() if args.mock else None)
    origin, results = run(args.guild_id, fetch)
    for result in results:
        print(result.line())
    failed = sum(not result.ok for result in results)
    if origin is None:
        # Fence, transport or endpoint refusal: nothing to attest, and no
        # transport label is claimed on a run that sent nothing.
        return 2 if any(r.name == "guild-fence" for r in results) else 1
    print(f"staging-guild E2E prep: {len(results) - failed}/{len(results)} checks passed"
          f" ({MOCK_TRANSPORT})")
    receipt = {"application_id": origin["application_id"],
               "guild_id": origin["guild_id"],
               "transport": MOCK_TRANSPORT,
               "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "commands": evidence_shape(results),
               "result": "pass" if not failed else "fail"}
    try:
        with open(args.evidence, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=2)
            handle.write("\n")
    except OSError as error:
        print(f"staging-guild E2E prep failed: evidence not written ({error.__class__.__name__})")
        return 1
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
