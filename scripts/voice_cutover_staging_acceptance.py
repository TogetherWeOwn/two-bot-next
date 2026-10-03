#!/usr/bin/env python3
"""Voice cutover staging QA acceptance gate (stdlib only; read-only; staging-only).

Preconditions (staging build healthy, staging guild) then cutover checks
(rooms create/move/delete readiness, no ghosts, denied-path copy
actionable), ending in one verdict line:

  QA <head sha>: PASS
  QA <head sha>: NEEDS WORK (failing steps cited)

Every network call is a GET. Nothing writes: no voice joins, no command
invocations, no database reads/writes from this workspace (staging databases
are never probed from an agent workspace; see docs/cutover.md). The live
room-practice and ghost-poll halves run only from signed receipts produced
by the staffed rehearsal (`--practice-receipt`) and the read-only
`report voice-ghosts` tool (`--ghost-receipt`); without them those rows are
recorded as blocked, never guessed.

Guild fence (fail-closed, before any request): the run proceeds only when
the guild id is exactly the TWO Staging guild. The live guild, any other
guild, or a missing id refuses with exit 2 and no request is sent. The
staging origin must be a two-bot-next-staging workers.dev host; anything
else refuses before any request.

Usage:
  DISCORD_STAGING_BOT_TOKEN=... DISCORD_STAGING_GUILD_ID=... \\
  STAGING_WORKER_URL=https://two-bot-next-staging.<sub>.workers.dev \\
      python3 scripts/voice_cutover_staging_acceptance.py \\
          --head-sha <40hex> [--evidence FILE] \\
          [--ci-check pass|fail] [--ci-deploy pass|fail] \\
          [--practice-receipt FILE] [--ghost-receipt FILE]
"""

import argparse
import http.client
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Pinned identity guards. Sources: crates/core/src/backup/guild_config.rs
# (TWO_STAGING_GUILD_ID, STAGING_BOT_APPLICATION_ID),
# crates/cutover/src/lib.rs (LIVE_GUILD_ID) and wrangler.toml
# (two-bot-next-staging workers.dev origin). No snowflake may be added here
# without updating those authorities first.
STAGING_GUILD_ID = "1545644954272137297"
LIVE_GUILD_ID = "326474832151838730"
STAGING_APPLICATION_ID = "1469137636663758888"

STAGING_HOST = re.compile(r"two-bot-next-staging\.[a-z0-9-]+\.workers\.dev")
HEAD_SHA = re.compile(r"[0-9a-f]{40}")

API = "https://discord.com/api/v10"
BODY_CAP = 64 << 10
TIMEOUT_SECONDS = 10
USER_AGENT = "two-bot-next-voice-cutover-acceptance/1.0 (read-only E2E)"

# Voice slash commands wired by voice_command_set (crates/bot/src/voice_rooms.rs
# over crates/core/src/voice_rooms.rs voice_commands()). The cutover practice
# needs at least create/setup/access published in the staging guild.
WIRED_VOICE_COMMANDS = (
    "create", "setup", "ping", "invite", "textchannels", "access",
    "reclaim", "transfer", "logging", "export", "import", "kick",
)

VOICE_CHANNEL_TYPES = frozenset({2, 13})  # GUILD_VOICE, GUILD_STAGE_VOICE

# Actionable denied-path copy (shipped by the interactions-denial slice on
# this head; see docs/interaction-replies.md). Every refusal must name the
# Discord permission, who grants it, or the admin-only enable path.
REQUIRED_ROUTER_COPY = (
    "Ask a server admin to enable them in the bot configuration "
    "— this is a host setting, not a Discord role.",
    "You need the Manage Server permission to use this command. "
    "Ask a server admin to grant it.",
    "You need the Manage Events permission to use this command. "
    "Ask a server admin to grant it.",
    "Ask a server moderator or admin to grant it.",
)
REQUIRED_REPLIES_COPY = (
    "pick it again from the / command list.",
    "Run the command again to get a fresh one.",
    "Please try again — if it keeps happening, "
    "share this reference with a server admin.",
)
# Stale literals the smoke contract must no longer pin (pre-denial-slice
# wording). Their presence means the contract contradicts the router.
STALE_CONTRACT_LITERALS = (
    '"This interaction is no longer available."',
    '"Manage Server permission is required."',
)


class GateError(Exception):
    """A check's one-line failure reason (fixed vocabulary only)."""


@dataclass(frozen=True)
class Result:
    name: str
    verdict: str  # pass | fail | blocked
    detail: str

    def line(self):
        return f"{self.verdict.upper()} {self.name}: {self.detail}"


def guild_fence(guild_id):
    """Allowlist exactly the TWO Staging guild; refuse everything else unread."""
    if not guild_id:
        raise GateError("refusing: no staging guild id given ($DISCORD_STAGING_GUILD_ID)")
    if guild_id == LIVE_GUILD_ID:
        raise GateError("refusing: live guild id must never be probed")
    if guild_id != STAGING_GUILD_ID:
        raise GateError("refusing: not the TWO Staging guild id")
    return guild_id


def require_token(token):
    if not token:
        raise GateError("refusing: no staging bot token given ($DISCORD_STAGING_BOT_TOKEN)")
    return token


def staging_origin(url):
    """Allowlist the staging Worker origin; refuse everything else unread."""
    if not url:
        raise GateError("no staging Worker URL given ($STAGING_WORKER_URL)")
    from urllib.parse import urlsplit
    try:
        parts = urlsplit(url)
    except ValueError:
        raise GateError("refusing: not the two-bot-next-staging workers.dev origin")
    try:
        port = parts.port
    except ValueError:
        port = "invalid"
    if (parts.scheme != "https" or port is not None or parts.username or parts.password
            or not STAGING_HOST.fullmatch(parts.hostname or "")
            or parts.path not in ("", "/") or parts.query or parts.fragment):
        raise GateError("refusing: not the two-bot-next-staging workers.dev origin")
    return f"https://{parts.hostname}"


def check_head_sha(value):
    if not value or not HEAD_SHA.fullmatch(value):
        raise GateError("refusing: --head-sha must be the full 40-hex commit")
    return value


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
                return response.status, dict(response.headers), response.read(BODY_CAP + 1)
        except urllib.error.HTTPError as error:
            return error.code, dict(error.headers or {}), error.read(BODY_CAP + 1)

    return fetch


def header(headers, name):
    if isinstance(headers, dict):
        for key, value in headers.items():
            if isinstance(key, str) and key.lower() == name and isinstance(value, str):
                return value
    return None


def check_health(origin, fetch_fn):
    """P1a: /health answers 200 with the process-ok shape."""
    try:
        status, headers, body = fetch_fn(origin + "/health")
    except (OSError, http.client.HTTPException) as error:
        return Result("P1a staging-health", "fail",
                      f"health probe did not respond ({error.__class__.__name__})",
                      )
    version = header(headers, "x-two-worker-version") or "unversioned"
    if status == 503 and b"ownership_fenced" in body:
        return Result("P1a staging-health", "fail",
                      f"health 503 ownership_fenced, worker {version}: "
                      "singleton fenced, no owner serving this build")
    if status != 200:
        return Result("P1a staging-health", "fail",
                      f"health answered {status}, expected 200")
    try:
        shape = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return Result("P1a staging-health", "fail", "health 200 with invalid JSON")
    if not isinstance(shape, dict) or shape.get("status") != "ok":
        return Result("P1a staging-health", "fail",
                      "health 200 without the process-ok shape")
    return Result("P1a staging-health", "pass",
                  f"health 200 process-ok, worker {version}")


def check_readyz(origin, head_sha, fetch_fn):
    """P1b: /readyz answers 200, every component ready, revision is the head."""
    try:
        status, headers, body = fetch_fn(origin + "/readyz")
    except (OSError, http.client.HTTPException) as error:
        return Result("P1b staging-readyz", "fail",
                      f"readyz probe did not respond ({error.__class__.__name__})")
    version = header(headers, "x-two-worker-version") or "unversioned"
    if status == 503 and b"ownership_fenced" in body:
        return Result("P1b staging-readyz", "fail",
                      f"readyz 503 ownership_fenced, worker {version}: "
                      "readiness cannot be proven while fenced")
    if status == 503:
        return Result("P1b staging-readyz", "fail",
                      "readyz 503: parked process, not a healthy staging build")
    if status != 200:
        return Result("P1b staging-readyz", "fail",
                      f"readyz answered {status}, expected 200")
    try:
        shape = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return Result("P1b staging-readyz", "fail", "readyz 200 with invalid JSON")
    components = shape.get("components") if isinstance(shape, dict) else None
    if not isinstance(components, list) or not components or any(
            not (isinstance(row, list) and len(row) == 2 and row[1] == "ready")
            for row in components):
        return Result("P1b staging-readyz", "fail",
                      "readyz 200 without every component ready")
    revision = shape.get("build_revision") if isinstance(shape, dict) else None
    build_id = shape.get("build_id") if isinstance(shape, dict) else None
    if revision != head_sha:
        return Result("P1b staging-readyz", "fail",
                      f"readyz 200 but build_revision is not the QA head "
                      f"(build {build_id})")
    return Result("P1b staging-readyz", "pass",
                  f"readyz 200 every component ready at the QA head (build {build_id})")


def discord_json(fetch_fn, url):
    try:
        status, _, body = fetch_fn(url)
    except (OSError, http.client.HTTPException) as error:
        raise GateError(f"interactions endpoint did not respond ({error.__class__.__name__})")
    if status == 401:
        raise GateError("credential refused (401); rotation is an operator decision")
    if status == 403:
        raise GateError("staging application lacks access (403)")
    if status == 429:
        raise GateError("rate limited (429); rerun later")
    if status != 200:
        raise GateError(f"interactions endpoint answered {status}, expected 200")
    try:
        return json.loads(body)
    except (ValueError, UnicodeDecodeError):
        raise GateError("interactions endpoint answered 200 with invalid JSON")


def check_guild_registry(guild, token, fetch_fn=None):
    """P2: token belongs to the staging app; record the published registry."""
    fetch = fetch_fn if fetch_fn is not None else make_fetch(token)
    me = discord_json(fetch, API + "/users/@me")
    if not isinstance(me, dict) or me.get("id") != STAGING_APPLICATION_ID:
        return [Result("P2a staging-guild-identity", "fail",
                       "refusing: token is not the staging application")], None
    entries = discord_json(
        fetch, f"{API}/applications/{STAGING_APPLICATION_ID}/guilds/{guild}/commands")
    if not isinstance(entries, list):
        return [Result("P2a staging-guild-identity", "pass",
                       "token is the staging application"), Result(
            "P2b voice-registry", "fail",
            "command list answered 200 without a list")], None
    names = sorted(c.get("name") for c in entries
                   if isinstance(c, dict) and isinstance(c.get("name"), str))
    results = [Result("P2a staging-guild-identity", "pass",
                      f"token is the staging application; "
                      f"{len(names)} guild commands published")]
    missing = [name for name in WIRED_VOICE_COMMANDS if name not in names]
    if missing:
        results.append(Result(
            "P2b voice-registry", "fail",
            f"staging registry publishes {len(names)} commands "
            f"({', '.join(names) if names else 'none'}) but no voice slice: "
            f"missing {', '.join(missing)}"))
    else:
        results.append(Result(
            "P2b voice-registry", "pass",
            f"all {len(WIRED_VOICE_COMMANDS)} wired voice commands published"))
    return results, names


def check_rooms_practice(p1_ok, voice_ok, practice_path):
    """C1: live create/move/delete practice runs only on a healthy staging
    guild, from a signed practice receipt. Never driven by this script."""
    if not p1_ok:
        return Result("C1 rooms-live-practice", "blocked",
                      "staging build not healthy: no voice joins attempted")
    if not voice_ok:
        return Result("C1 rooms-live-practice", "blocked",
                      "voice slice unpublished in staging: practice has no commands")
    if not practice_path:
        return Result("C1 rooms-live-practice", "blocked",
                      "healthy staging but no --practice-receipt: staffed "
                      "create/move/delete run per rehearsal section 4 not supplied")
    try:
        receipt = json.loads(Path(practice_path).read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return Result("C1 rooms-live-practice", "fail",
                      "practice receipt unreadable: not evidence of a live run")
    if not isinstance(receipt, dict):
        return Result("C1 rooms-live-practice", "fail",
                      "practice receipt unreadable: not evidence of a live run")
    steps = receipt.get("steps") if isinstance(receipt.get("steps"), dict) else {}
    wanted = ("create", "move", "delete")
    bad = [name for name in wanted if steps.get(name) != "pass"]
    if bad or receipt.get("result") != "pass":
        return Result("C1 rooms-live-practice", "fail",
                      f"practice receipt fails: {', '.join(bad) if bad else 'result not pass'}")
    return Result("C1 rooms-live-practice", "pass",
                  "practice receipt passes create/move/delete on staging")


def ghost_selftest():
    """C2a: the tracked-vs-live set arithmetic on the report's seed fixture
    (mirrors voice_ghosts build_seed_ghost_data; proves the script reads the
    contract, not staging state)."""
    tracked = {101, 102, 103}
    live = {101, 102, 201}
    present = sorted(tracked & live)
    gone = sorted(tracked - live)
    untracked = sorted(live - tracked)
    if (present, gone, untracked) != ([101, 102], [103], [201]):
        return Result("C2a ghost-logic-selftest", "fail",
                      "seed diff misclassified: script logic wrong")
    return Result("C2a ghost-logic-selftest", "pass",
                  "seed diff classifies present/gone/untracked exactly")


def check_ghost_receipt(p1_ok, ghost_path):
    """C2b: the live ghost poll reads only a signed `report voice-ghosts`
    receipt. Staging databases are never queried from this workspace."""
    if not p1_ok:
        return Result("C2b ghost-live-poll", "blocked",
                      "staging build not healthy: ghost poll needs a serving owner")
    if not ghost_path:
        return Result("C2b ghost-live-poll", "blocked",
                      "no --ghost-receipt: live `report voice-ghosts` poll not supplied; "
                      "staging databases are never read from this workspace")
    try:
        receipt = json.loads(Path(ghost_path).read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return Result("C2b ghost-live-poll", "fail",
                      "ghost receipt unreadable: not evidence of a clean poll")
    if not isinstance(receipt, dict) or receipt.get("tool") != "voice-ghosts":
        return Result("C2b ghost-live-poll", "fail",
                      "ghost receipt unreadable: not evidence of a clean poll")
    gone = receipt.get("tracked_gone") or []
    untracked = receipt.get("untracked_present") or []
    if gone or untracked or receipt.get("clean") is not True:
        return Result("C2b ghost-live-poll", "fail",
                      f"ghost poll not clean: tracked_gone={len(gone)} "
                      f"untracked_present={len(untracked)}")
    return Result("C2b ghost-live-poll", "pass",
                  f"ghost poll clean over {receipt.get('tracked_rooms')} tracked rooms")


def read_source(root, *parts):
    try:
        return (root.joinpath(*parts)).read_text(encoding="utf-8")
    except OSError:
        return ""


def check_denied_copy(root):
    """C3: the shipped denial slice names a next step everywhere, and the
    smoke contract pins the same copy (a stale contract fails CI)."""
    router = read_source(root, "crates", "core", "src", "router.rs")
    replies = read_source(root, "crates", "core", "src", "router", "replies.rs")
    contract = read_source(root, "crates", "bot", "src", "smoke_error_contract_tests.rs")
    results = []
    missing_router = [text for text in REQUIRED_ROUTER_COPY if text not in router]
    missing_replies = [text for text in REQUIRED_REPLIES_COPY if text not in replies]
    if missing_router or missing_replies:
        parts = []
        if missing_router:
            parts.append(f"router refusals miss {len(missing_router)} actionable texts")
        if missing_replies:
            parts.append("reply lifecycle misses the split unknown/expired copy")
        results.append(Result("C3a router-denied-copy", "fail", "; ".join(parts)))
    else:
        results.append(Result("C3a router-denied-copy", "pass",
                              "permission denials name the Discord permission and granter; "
                              "disabled features name the host-setting enable path; "
                              "unknown vs expired controls split with next steps"))
    stale = [text for text in STALE_CONTRACT_LITERALS if text in contract]
    if stale:
        split = contract.splitlines()
        lines = []
        for text in stale:
            # Prefer the executable assert pinning the stale copy; the
            # doc-table mention alone does not fail CI.
            for i, line in enumerate(split, 1):
                if text.strip('"') in line and "assert" in line:
                    lines.append(str(i))
                    break
            else:
                for i, line in enumerate(split, 1):
                    if text.strip('"') in line:
                        lines.append(str(i))
                        break
        results.append(Result(
            "C3b smoke-contract-in-sync", "fail",
            f"smoke contract still pins pre-slice copy "
            f"(lines {', '.join(lines)}): check stays red until updated"))
    elif ("UNKNOWN_COMMAND_REPLY" not in contract
            and "EXPIRED_COMPONENT_REPLY" not in contract
            and "RouterRefusal::" not in contract):
        results.append(Result("C3b smoke-contract-in-sync", "fail",
                              "smoke contract pins neither the stale nor the current copy"))
    else:
        results.append(Result("C3b smoke-contract-in-sync", "pass",
                              "smoke contract pins the shipped denial copy"))
    voice = read_source(root, "crates", "bot", "src", "voice_rooms.rs")
    if ('You need the server\'s required role to use voice-room commands.' in voice
            and 'You do not have a role that may use this command here.' in voice):
        results.append(Result(
            "C3c voice-access-denied-copy", "fail",
            "advisory, pre-existing: /access denials name no role and no "
            "granter; route them through the actionable-copy bar"))
    else:
        results.append(Result("C3c voice-access-denied-copy", "pass",
                              "voice access denials name a next step"))
    return results


def run(options, fetch_fn=None, root=ROOT):
    """Fence first, then preconditions, then checks. No writes anywhere."""
    try:
        guild = guild_fence(options.guild_id)
        require_token(options.token)
        origin = staging_origin(options.staging_url)
        head_sha = check_head_sha(options.head_sha)
    except GateError as error:
        return None, [Result("gate-fence", "blocked", str(error))]
    fetch = fetch_fn if fetch_fn is not None else make_fetch(options.token)
    results = [check_health(origin, fetch), check_readyz(origin, head_sha, fetch)]
    p1_ok = all(r.verdict == "pass" for r in results)
    try:
        registry_results, _ = check_guild_registry(guild, options.token, fetch)
        results.extend(registry_results)
    except GateError as error:
        results.append(Result("P2 staging-guild", "fail", str(error)))
        registry_results = []
    voice_ok = any(r.name == "P2b voice-registry" and r.verdict == "pass"
                   for r in registry_results)
    results.append(check_rooms_practice(p1_ok, voice_ok, options.practice_receipt))
    results.append(ghost_selftest())
    results.append(check_ghost_receipt(p1_ok, options.ghost_receipt))
    results.extend(check_denied_copy(root))
    for label, conclusion in (("CI check", options.ci_check),
                              ("CI deploy-staging", options.ci_deploy)):
        if conclusion == "pass":
            results.append(Result(label, "pass", "runner-observed green on the QA head"))
        elif conclusion == "fail":
            results.append(Result(label, "fail", "runner-observed red on the QA head"))
        else:
            results.append(Result(label, "blocked", "CI state not supplied to this run"))
    return {"origin": origin, "guild_id": guild}, results


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--head-sha", required=True,
                        help="full 40-hex commit under QA")
    parser.add_argument("--staging-url",
                        default=os.environ.get("STAGING_WORKER_URL")
                        or os.environ.get("STAGING_URL"),
                        help="staging Worker origin (default: $STAGING_WORKER_URL)")
    parser.add_argument("--guild-id",
                        default=os.environ.get("DISCORD_STAGING_GUILD_ID"),
                        help="staging guild id (default: $DISCORD_STAGING_GUILD_ID)")
    parser.add_argument("--evidence",
                        default="voice-cutover-staging-acceptance-evidence.json",
                        help="receipt path written when the fetch phase completes")
    parser.add_argument("--ci-check", choices=("pass", "fail"),
                        default=None, help="runner-observed check conclusion on the head")
    parser.add_argument("--ci-deploy", choices=("pass", "fail"),
                        default=None, help="runner-observed deploy-staging conclusion on the head")
    parser.add_argument("--practice-receipt", default=None,
                        help="signed staffed room-practice receipt JSON")
    parser.add_argument("--ghost-receipt", default=None,
                        help="signed `report voice-ghosts` receipt JSON")
    return parser.parse_args(argv)


def main(argv=None, fetch_fn=None, root=ROOT):
    args = parse_args(argv)
    options = argparse.Namespace(
        guild_id=args.guild_id,
        token=os.environ.get("DISCORD_STAGING_BOT_TOKEN", ""),
        staging_url=args.staging_url,
        head_sha=args.head_sha,
        practice_receipt=args.practice_receipt,
        ghost_receipt=args.ghost_receipt,
        ci_check=args.ci_check,
        ci_deploy=args.ci_deploy,
    )
    origin, results = run(options, fetch_fn, root)
    for result in results:
        print(result.line())
    bad = [r for r in results if r.verdict != "pass"]
    verdict = "PASS" if not bad else "NEEDS WORK"
    print(f"QA {args.head_sha}: {verdict}"
          + ("" if not bad else f" ({', '.join(r.name for r in bad)})"))
    print(f"voice cutover staging acceptance: "
          f"{len(results) - len(bad)}/{len(results)} checks passed")
    if origin is None:
        return 2  # fence/credential/usage refusal: nothing to attest
    receipt = {"utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "head_sha": args.head_sha,
               "origin": origin["origin"],
               "guild_id": origin["guild_id"],
               "checks": [{"check": r.name, "verdict": r.verdict,
                           "detail": r.detail} for r in results],
               "result": "pass" if verdict == "PASS" else "needs_work"}
    try:
        with open(args.evidence, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=2)
            handle.write("\n")
    except OSError as error:
        print(f"acceptance failed: evidence not written ({error.__class__.__name__})")
        return 1
    return 0 if verdict == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
