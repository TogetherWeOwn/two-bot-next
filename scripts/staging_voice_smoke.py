#!/usr/bin/env python3
"""Read-only staging-guild voice smoke harness: room-lifecycle probes (stdlib only).

Readiness scaffolding for the staging-guild voice room-lifecycle smoke. It
proves the preconditions the full two-human smoke will drive the moment the
voice command set is registered, and stays green (with SKIP verdicts) until
then. It never writes, never touches production, and sends no credentials
anywhere except the Authorization header Discord itself requires.

Phases, in order:

  worker     optional staging Worker health/readyz (explicit agent, no
             redirect, no credentials). Skipped when no --staging-url is
             given; fails on an ownership refusal or a mistruthful breakdown.
  registry   staging guild command list plus one per-command resource read
             for the control (rank) and every voice surface below.
  lifecycle  four room-lifecycle scaffolding probes evaluated against the
             registry result (no extra network):

    creator-create   needs /create; shape-checks its required name option.
    join-move        needs /create; voice-state join/move itself needs a
                     second human account in the staging voice channel, so
                     this probe attests scaffolding readiness only.
    kick-ballot      needs /kick in its voice (member) or moderation
                     (target, first-wins) shape; the ballot runs only after
                     an eligible moderation refusal in the latter case.
    room-cleanup     needs /create; attests the empty-room reconcile
                     expectation the full smoke will verify in the client.

The 17 pinned voice names are the freeze-receipt publish order (create,
setup, ping, invite, textchannels, access, reclaim, transfer, logging,
export, import, position, group, inheritpermissions, defaultlimit,
alwaysprivate, kick). The five newer room controls (name, private, public,
limit, unlimit) ride along as extended surfaces so the harness stays
accurate whether or not the pending registration includes them.

Guild fence (fail-closed, before any request): the run proceeds only when
the guild id is exactly the TWO Staging guild. The live guild, any other
guild, or a missing id refuses with exit 2 and no request is sent. The
Worker origin fence allows only the two-bot-next-staging workers.dev
origin; anything else refuses the same way.

Usage (live, read-only GETs):
  DISCORD_STAGING_BOT_TOKEN=... DISCORD_STAGING_GUILD_ID=... \\
      python3 scripts/staging_voice_smoke.py [--staging-url URL] [--evidence FILE]

Usage (offline fixtures, no network, no token):
  python3 scripts/staging_voice_smoke.py --mock [--evidence FILE]
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
from urllib.parse import urlsplit

# Pinned identity guards. Sources: crates/core/src/backup/guild_config.rs
# (TWO staging guild id, staging bot application id) and
# crates/cutover/src/lib.rs (live guild id). No snowflake may be added here
# without updating those authorities first.
STAGING_GUILD_ID = "1545644954272137297"
LIVE_GUILD_ID = "326474832151838730"
STAGING_APPLICATION_ID = "1469137636663758888"

API = "https://discord.com/api/v10"
CONTROL_COMMAND = "rank"
# Freeze-receipt publish order for the pending voice registration.
VOICE_COMMANDS = (
    "create", "setup", "ping", "invite", "textchannels", "access",
    "reclaim", "transfer", "logging", "export", "import", "position",
    "group", "inheritpermissions", "defaultlimit", "alwaysprivate", "kick",
)
# Newer room controls present in the current source; reported separately.
EXTENDED_COMMANDS = ("name", "private", "public", "limit", "unlimit")

BODY_CAP = 64 << 10
TIMEOUT_SECONDS = 10
DISCORD_USER_AGENT = "two-bot-next-staging-voice-smoke/1.0 (read-only)"
WORKER_USER_AGENT = "two-bot-next-staging-rollout/1.0"
STAGING_HOST = re.compile(r"two-bot-next-staging\.[a-z0-9-]+\.workers\.dev")
MOCK_TRANSPORT = "mock-local-fixtures"

# Pending-registration marker carried on SKIP verdicts (fixed vocabulary).
AWAITING_REGISTRATION = "awaiting voice command registration"


class SmokeError(Exception):
    """A check's one-line failure reason (fixed vocabulary only)."""


@dataclass(frozen=True)
class Result:
    name: str
    verdict: str  # "pass", "skip" or "fail"
    reason: str
    command_id: str | None = None
    version: str | None = None

    def line(self):
        return f"{self.verdict.upper()} {self.name}: {self.reason}"


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


def staging_origin(url):
    """Allowlist the staging Worker origin; refuse everything else unread."""
    if not url:
        raise SmokeError("no staging Worker URL given (--staging-url or $STAGING_WORKER_URL)")
    try:
        parts = urlsplit(url)
    except ValueError:
        raise SmokeError("refusing: not the two-bot-next-staging workers.dev origin")
    try:
        port = parts.port
    except ValueError:
        raise SmokeError("refusing: not the two-bot-next-staging workers.dev origin")
    if (parts.scheme != "https" or port is not None or parts.username or parts.password
            or not STAGING_HOST.fullmatch(parts.hostname or "")
            or parts.path not in ("", "/") or parts.query or parts.fragment):
        raise SmokeError("refusing: not the two-bot-next-staging workers.dev origin")
    return f"https://{parts.hostname}"


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None  # urllib then raises the 3xx as an HTTPError


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
    """Build the only Discord network function; the token never leaves this closure."""

    def fetch(url):
        request = urllib.request.Request(
            url,
            method="GET",
            headers={"Authorization": "Bot " + token,
                     "User-Agent": DISCORD_USER_AGENT,
                     "Cache-Control": "no-cache"},
        )
        try:
            with urllib.request.build_opener(_RefuseRedirect).open(
                    request, timeout=TIMEOUT_SECONDS) as response:
                return response.status, response.read(BODY_CAP + 1)
        except urllib.error.HTTPError as error:
            return error.code, error.read(BODY_CAP + 1)

    return fetch


def fetch_worker(url):
    """One Worker GET without redirects; returns (status, body bytes)."""
    opener = urllib.request.build_opener(_NoRedirect)
    request = urllib.request.Request(url, method="GET",
                                     headers={"User-Agent": WORKER_USER_AGENT,
                                              "Cache-Control": "no-cache"})
    try:
        with opener.open(request, timeout=TIMEOUT_SECONDS) as response:
            return response.status, response.read(BODY_CAP + 1)
    except urllib.error.HTTPError as error:
        return error.code, error.read(BODY_CAP + 1)


def get_json(fetch_fn, url):
    """One GET returning parsed JSON; transport/auth failures are SmokeErrors."""
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
        return json.loads(body)
    except (ValueError, UnicodeDecodeError):
        raise SmokeError("interactions endpoint answered 200 with invalid JSON")


def check_identity(fetch_fn):
    """The token must belong to the staging application; anything else refuses."""
    me = get_json(fetch_fn, API + "/users/@me")
    if not isinstance(me, dict) or me.get("id") != STAGING_APPLICATION_ID:
        raise SmokeError("refusing: token is not the staging application")
    return me["id"]


# Fixed vocabulary for the one registry outcome that is a SKIP for voice
# surfaces: the list entry is absent (registration pending). Every other
# FAIL (no id, disagreeing resource, wrong guild scope) is a real defect.
NOT_REGISTERED = "not registered in the staging guild"


def check_command(fetch_fn, app_id, guild_id, entries, name):
    """Read one command's registration: list entry plus its own resource."""
    entry = next((c for c in entries if isinstance(c, dict) and c.get("name") == name), None)
    if entry is None:
        return Result(name, "fail", NOT_REGISTERED), None
    command_id = entry.get("id")
    if not isinstance(command_id, str) or not command_id:
        return Result(name, "fail", "registered entry carries no command id"), None
    detail = get_json(
        fetch_fn,
        f"{API}/applications/{app_id}/guilds/{guild_id}/commands/{command_id}",
    )
    if not isinstance(detail, dict) or detail.get("name") != name:
        return Result(name, "fail", "command resource disagrees with the list entry",
                      command_id=command_id), None
    if detail.get("guild_id") != guild_id:
        return Result(name, "fail", "command resource is not scoped to the staging guild",
                      command_id=command_id), None
    version = detail.get("version")
    version_text = version if isinstance(version, str) and version else "unversioned"
    return (Result(name, "pass",
                   f"registered in the staging guild (version {version_text})",
                   command_id=command_id,
                   version=version if isinstance(version, str) else None),
            detail)


def option_named(detail, option_name, option_type=None):
    """Find one option by name (and optional Discord type int) in a command detail."""
    options = detail.get("options") if isinstance(detail, dict) else None
    if not isinstance(options, list):
        return None
    for option in options:
        if not isinstance(option, dict) or option.get("name") != option_name:
            continue
        if option_type is not None and option.get("type") != option_type:
            continue
        return option
    return None


def check_worker(origin):
    """Read-only Worker liveness + readiness shape; refusals and lies fail."""
    try:
        status, body = fetch_worker(origin + "/health")
    except (OSError, http.client.HTTPException) as error:
        return [Result("worker-health", "fail",
                       f"GET /health did not respond ({error.__class__.__name__})")], None
    if len(body) > BODY_CAP:
        return [Result("worker-health", "fail", "GET /health answered over the body cap")], None
    try:
        health = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return [Result("worker-health", "fail", "GET /health answered without JSON")], None
    results = []
    if status == 200 and health == {"status": "ok"}:
        results.append(Result("worker-health", "pass", "GET /health 200 {\"status\": \"ok\"}"))
    else:
        results.append(Result("worker-health", "fail",
                              f"GET /health answered {status} without {{\"status\": \"ok\"}}"))
        return results, None
    try:
        status, body = fetch_worker(origin + "/readyz")
    except (OSError, http.client.HTTPException) as error:
        results.append(Result("worker-readyz", "fail",
                              f"GET /readyz did not respond ({error.__class__.__name__})"))
        return results, None
    if len(body) > BODY_CAP:
        results.append(Result("worker-readyz", "fail", "GET /readyz answered over the body cap"))
        return results, None
    try:
        report = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        results.append(Result("worker-readyz", "fail", "GET /readyz answered without JSON"))
        return results, None
    if isinstance(report, dict) and report.get("error"):
        results.append(Result("worker-readyz", "fail",
                              f"GET /readyz refused with error {report.get('error')!r:.40}"))
        return results, None
    components = report.get("components") if isinstance(report, dict) else None
    if (not isinstance(components, list) or not components
            or not all(isinstance(c, list) and len(c) == 2
                       and isinstance(c[0], str) and c[0]
                       and c[1] in ("ready", "starting", "down") for c in components)):
        results.append(Result("worker-readyz", "fail",
                              "GET /readyz answered without the components breakdown"))
        return results, None
    state = dict(components)
    ready = all(value == "ready" for value in state.values())
    expected = 200 if ready else 503
    if status != expected:
        results.append(Result("worker-readyz", "fail",
                              f"GET /readyz {status} disagrees with its breakdown"))
        return results, None
    build = ""
    if isinstance(report.get("build_revision"), str):
        build = f" (build {report['build_revision']:.20})"
    summary = ", ".join(f"{k}={v}" for k, v in state.items())
    results.append(Result("worker-readyz", "pass", f"GET /readyz {status}: {summary}{build}"))
    return results, {"ready": ready, "components": state,
                     "build_revision": report.get("build_revision")
                     if isinstance(report.get("build_revision"), str) else None}


def lifecycle_probes(details_by_name, present_names):
    """Evaluate the four scaffolding probes against the registry result (no I/O).

    `present_names` carries every command the list entry named, whether or
    not its resource read succeeded; `details_by_name` carries only the
    resources that read clean. A present-but-unreadable command is a FAIL in
    the registry phase, so the lifecycle probes must not report it as
    merely awaiting registration.
    """
    probes = []

    def registered(name):
        return name in present_names

    # A present-but-unreadable command (registry FAIL above) fails its
    # lifecycle probes too: scaffolding is not ready until the registry is.
    unreadable = "is registered but its resource did not read clean; see the registry phase"
    # 1. creator-channel create: needs /create with its required name option.
    if not registered("create"):
        probes.append(Result("creator-create", "skip",
                             f"/create {AWAITING_REGISTRATION}; full smoke reruns on arrival"))
    elif "create" not in details_by_name:
        probes.append(Result("creator-create", "fail", f"/create {unreadable}"))
    else:
        option = option_named(details_by_name["create"], "name", 3)
        if option is not None and option.get("required") is True:
            probes.append(Result("creator-create", "pass",
                                 "/create shape ready (required name option); full smoke reruns on arrival"))
        else:
            probes.append(Result("creator-create", "fail",
                                 "/create registered without its required name option"))

    # 2. join/move: gateway voice-state, needs a second human account; the
    # harness attests scaffolding readiness once /create exists.
    if not registered("create"):
        probes.append(Result("join-move", "skip",
                             f"/create {AWAITING_REGISTRATION}; join/move needs a second human account"))
    elif "create" not in details_by_name:
        probes.append(Result("join-move", "fail", f"/create {unreadable}"))
    else:
        probes.append(Result("join-move", "pass",
                             "scaffolding ready (/create present); live join/move needs a second human account"))

    # 3. kick vote-ballot: needs /kick in either known shape. Moderation's
    # /kick (required `target`) wins the first-wins merge, so the registered
    # command is moderation's whenever moderation publishes; the voice ballot
    # then runs only after an eligible moderation refusal (router-first).
    if not registered("kick"):
        probes.append(Result("kick-ballot", "skip",
                             f"/kick {AWAITING_REGISTRATION}; ballot scaffolding reruns on arrival"))
    else:
        detail = details_by_name.get("kick")
        voice = option_named(detail, "member", 6)
        moderation = option_named(detail, "target", 6)
        if voice is not None and voice.get("required") is True:
            probes.append(Result("kick-ballot", "pass",
                                 "/kick shape ready (voice member option); ballot scaffolding reruns on arrival"))
        elif moderation is not None and moderation.get("required") is True:
            probes.append(Result("kick-ballot", "pass",
                                 "/kick is moderation's (target option, first-wins); "
                                 "ballot runs after an eligible moderation refusal"))
        else:
            probes.append(Result("kick-ballot", "fail",
                                 "/kick registered with neither the voice nor the moderation shape"))

    # 4. room cleanup: empty-room reconcile once rooms exist; needs /create.
    if not registered("create"):
        probes.append(Result("room-cleanup", "skip",
                             f"/create {AWAITING_REGISTRATION}; empty-room reconcile reruns on arrival"))
    elif "create" not in details_by_name:
        probes.append(Result("room-cleanup", "fail", f"/create {unreadable}"))
    else:
        probes.append(Result("room-cleanup", "pass",
                             "scaffolding ready (/create present); empty-room reconcile reruns in the full smoke"))
    return probes


def fixture_entry(index, name):
    """One mock registry entry with a stable fake id and version."""
    return {"id": f"{3000000000000000000 + index}", "name": name, "type": 1,
            "guild_id": STAGING_GUILD_ID, "version": f"{4000000000000000000 + index}",
            "options": MOCK_OPTIONS.get(name, [])}


MOCK_OPTIONS = {
    "create": [{"name": "name", "description": "Name for the new creator channel",
                "type": 3, "required": True, "max_length": 100}],
    "kick": [{"name": "member", "description": "Room occupant to put to a vote",
              "type": 6, "required": True},
             {"name": "reason", "description": "Why the vote was started",
              "type": 3, "max_length": 512}],
}


def fixture_commands():
    """Mock registry: control plus the full voice and extended surfaces."""
    names = [CONTROL_COMMAND, *VOICE_COMMANDS, *EXTENDED_COMMANDS]
    return [fixture_entry(index, name) for index, name in enumerate(names)]


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
            raise SmokeError(f"interactions endpoint has no fixture route for {url}")
        return routes[url]

    return fetch


def run(guild_id, token, fetch_fn, worker_url=None, worker_fetch=None):
    """Fence first, then Worker (optional), identity, registry, lifecycle."""
    try:
        guild = guild_fence(guild_id)
    except SmokeError as error:
        return None, [Result("guild-fence", "fail", str(error))]
    worker_results = []
    worker_info = None
    if worker_url:
        try:
            origin = staging_origin(worker_url)
        except SmokeError as error:
            return None, [Result("worker-origin", "fail", str(error))]
        if worker_fetch is not None:
            worker_results, worker_info = worker_fetch(origin)
        else:
            worker_results, worker_info = check_worker(origin)
        if any(r.verdict == "fail" for r in worker_results):
            return {"application_id": STAGING_APPLICATION_ID, "guild_id": guild,
                    "worker": worker_info}, worker_results
    else:
        worker_results = [Result("worker", "skip", "no staging Worker URL given; registry-only run")]
    # The mock transport carries no credential; every live path requires one,
    # even when tests inject a fake fetch (mirrors the automation read smoke).
    if token != "mock":
        try:
            require_token(token)
        except SmokeError as error:
            return None, [Result("guild-fence", "fail", str(error))]
    fetch = fetch_fn if fetch_fn is not None else make_fetch(token)
    try:
        app_id = check_identity(fetch)
        entries = get_json(fetch, f"{API}/applications/{app_id}/guilds/{guild}/commands")
        if not isinstance(entries, list):
            raise SmokeError("command list answered 200 without a list")
        command_results = []
        details_by_name = {}
        present_names = set()
        for name in (CONTROL_COMMAND, *VOICE_COMMANDS, *EXTENDED_COMMANDS):
            result, detail = check_command(fetch, app_id, guild, entries, name)
            if result.reason != NOT_REGISTERED:
                present_names.add(name)
            # Voice surfaces are pending registration: only the absent entry
            # is a SKIP. A present-but-wrong registration stays a FAIL.
            if (result.verdict == "fail" and name != CONTROL_COMMAND
                    and result.reason == NOT_REGISTERED):
                result = Result(name, "skip", f"{result.reason} ({AWAITING_REGISTRATION})")
            command_results.append(result)
            if detail is not None:
                details_by_name[name] = detail
    except SmokeError as error:
        return None, worker_results + [Result("interactions", "fail", str(error))]
    return ({"application_id": app_id, "guild_id": guild, "worker": worker_info},
            worker_results + command_results + lifecycle_probes(details_by_name, present_names))


def evidence_shape(results):
    """Allowlisted receipt: names, verdicts and shape tokens only."""
    return [{"check": r.name, "verdict": r.verdict, "detail": r.reason,
             "command_id": r.command_id, "version": r.version} for r in results]


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--guild-id",
                        default=os.environ.get("DISCORD_STAGING_GUILD_ID"),
                        help="staging guild id (default: $DISCORD_STAGING_GUILD_ID)")
    parser.add_argument("--staging-url",
                        default=os.environ.get("STAGING_WORKER_URL"),
                        help="staging Worker origin (default: $STAGING_WORKER_URL; omit for registry-only)")
    parser.add_argument("--mock", action="store_true",
                        help="drive the endpoint shape against local fixtures "
                             "(no network, no token; ignores --staging-url)")
    parser.add_argument("--evidence", default="staging-voice-smoke-evidence.json",
                        help="receipt path written when the fetch phase completes")
    return parser.parse_args(argv)


def main(argv=None, fetch_fn=None, worker_fetch=None):
    args = parse_args(argv)
    token = os.environ.get("DISCORD_STAGING_BOT_TOKEN", "")
    if args.mock:
        # Mock mode is fully offline: --staging-url is ignored so the
        # evidence label never covers live Worker GETs.
        fetch = fetch_fn if fetch_fn is not None else fixture_fetch()
        origin, results = run(args.guild_id or STAGING_GUILD_ID, "mock", fetch,
                              None, None)
        transport = MOCK_TRANSPORT
    else:
        if fetch_fn is None and not token:
            origin, results = run(args.guild_id, token, None, args.staging_url, worker_fetch)
        else:
            fetch = fetch_fn if fetch_fn is not None else make_fetch(token)
            origin, results = run(args.guild_id, token, fetch, args.staging_url, worker_fetch)
        transport = "live-discord-gets"
    for result in results:
        print(result.line())
    passed = sum(r.verdict == "pass" for r in results)
    skipped = sum(r.verdict == "skip" for r in results)
    failed = sum(r.verdict == "fail" for r in results)
    print(f"staging voice smoke: {passed} pass, {skipped} skip, {failed} fail ({transport})")
    if origin is None:
        # Fence, credential or endpoint refusal: nothing to attest.
        fence = any(r.name in ("guild-fence", "worker-origin") for r in results)
        return 2 if fence else 1
    worker_url = None if args.mock else (args.staging_url or None)
    receipt = {"application_id": origin["application_id"],
               "guild_id": origin["guild_id"],
               "transport": transport,
               "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "worker_url": worker_url,
               "checks": evidence_shape(results),
               "result": "pass" if not failed else "fail"}
    try:
        with open(args.evidence, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=2)
            handle.write("\n")
    except OSError as error:
        print(f"staging voice smoke failed: evidence not written ({error.__class__.__name__})")
        return 1
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
