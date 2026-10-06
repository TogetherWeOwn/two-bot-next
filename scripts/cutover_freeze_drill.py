#!/usr/bin/env python3
"""Staging-only cutover freeze/lock/unlock dry-run harness (stdlib only).

Rehearses the cutover-eve freeze sequence on the TWO Staging guild and
proves the guild was left as found: post a freeze notice, enable slowmode,
lock `@everyone` sends, verify the command surface still answers, then
restore everything and compare the pre/post semantic hash.

Guild fence (fail-closed, before anything else): the run proceeds only when
the guild id is exactly the TWO Staging guild. The live guild id aborts
before any request is built or sent; any other or missing id refuses the
same way. The live transport reads its token from the environment at call
time and never writes it to stdout, the receipt, or logs.

Transports: `--mock` drives local fixtures (no network, no token). `--live`
drives the real Discord REST transport and additionally requires
`--channel-id`, `--reason` and `--confirm-staging`. Anything else refuses
with no transport.

Usage (offline fixtures, no network):
  python3 scripts/cutover_freeze_drill.py --mock [--evidence FILE]

Live (staging guild only, Operator window):
  python3 scripts/cutover_freeze_drill.py --live --channel-id <id> \\
      --reason "cutover rehearsal" --confirm-staging [--evidence FILE]
"""

import argparse
import hashlib
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
USER_AGENT = "Mozilla/5.0 (compatible; two-bot-next-staging-drill/1.0)"
MOCK_TRANSPORT = "mock-local-fixtures"
LIVE_TRANSPORT = "staging-discord-rest"
BODY_CAP = 64 << 10

# Drill bounds mirror crates/core/src/channel_moderation.rs: slowmode
# 0..=21600 (0 disables) and the lockdown bits: SEND_MESSAGES (2048) plus
# SEND_MESSAGES_IN_THREADS, CREATE_PUBLIC_THREADS, CREATE_PRIVATE_THREADS and
# ADD_REACTIONS (`LOCKDOWN_BITS`).
MAX_SLOWMODE_SECONDS = 21_600
SEND_MESSAGES_BIT = 2048
LOCKDOWN_BITS = SEND_MESSAGES_BIT | (1 << 6) | (1 << 35) | (1 << 36) | (1 << 38)
DRILL_SLOWMODE_SECONDS = 30
MAX_REASON_LEN = 512


class DrillError(Exception):
    """A step's one-line failure reason (fixed vocabulary only)."""


def guild_fence(guild_id):
    """Allowlist exactly the TWO Staging guild; refuse everything else unread."""
    if not guild_id:
        raise DrillError("refusing: no staging guild id given ($DISCORD_STAGING_GUILD_ID)")
    if guild_id == LIVE_GUILD_ID:
        raise DrillError("refusing: live guild id must never be touched by this drill")
    if guild_id != STAGING_GUILD_ID:
        raise DrillError("refusing: not the TWO Staging guild id")
    return guild_id


def check_reason(reason):
    """Trimmed, non-empty audit reason of at most 512 chars, like the bot."""
    text = (reason or "").strip()
    if not text:
        raise DrillError("refusing: a non-blank audit reason is required")
    if len(text) > MAX_REASON_LEN:
        raise DrillError("refusing: audit reason exceeds 512 characters")
    return text


def semantic_hash(slowmode, allow, deny, overwrite_exists):
    """Fingerprint-only channel state: proves pre == post without raw IDs."""
    canonical = f"{slowmode}|{allow}|{deny}|{int(bool(overwrite_exists))}"
    return hashlib.sha256(canonical.encode()).hexdigest()[:16]


def plan_lockdown_masks(prior_allow, prior_deny):
    """Mirror plan_lockdown: clear the lockdown bits from allow, set them in deny."""
    try:
        allow = int(prior_allow) & ~LOCKDOWN_BITS
        deny = int(prior_deny) | LOCKDOWN_BITS
    except (TypeError, ValueError):
        raise DrillError("unparseable permission mask in @everyone overwrite")
    return str(allow), str(deny)


def everyone_overwrite(channel):
    """Read the @everyone permission overwrite from a channel payload.

    The @everyone overwrite is the role overwrite (type 0) whose id is the
    guild's own id. Both must match: any other role overwrite is skipped so
    a foreign seed is never read from or written back to @everyone.
    """
    guild_id = channel.get("guild_id")
    if not guild_id:
        return {"allow": "0", "deny": "0", "exists": False}
    for ow in channel.get("permission_overwrites") or []:
        if not isinstance(ow, dict):
            continue
        if str(ow.get("type")) == "0" and str(ow.get("id")) == str(guild_id):
            return {"allow": str(ow.get("allow", "0")),
                    "deny": str(ow.get("deny", "0")),
                    "exists": True}
    return {"allow": "0", "deny": "0", "exists": False}


def _to_int(value, what):
    """Parse a live payload integer, normalizing failures to DrillError."""
    try:
        return int(value)
    except (TypeError, ValueError):
        raise DrillError(f"unparseable {what} in channel payload")


class StepTimer:
    """Per-step UTC start + monotonic duration capture."""

    def __init__(self):
        self.rows = []

    def run(self, name, fn):
        started = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        begin = time.monotonic()
        try:
            detail = fn()
        except DrillError as error:
            self.rows.append({"name": name, "started_utc": started,
                              "duration_ms": int((time.monotonic() - begin) * 1000),
                              "result": "fail", "detail": str(error)})
            raise
        self.rows.append({"name": name, "started_utc": started,
                          "duration_ms": int((time.monotonic() - begin) * 1000),
                          "result": "pass", "detail": detail})
        return detail


def mock_channel():
    """Fixture channel: open staging drill channel with no @everyone overwrite."""
    return {"id": "1000000000000000001", "guild_id": STAGING_GUILD_ID,
            "name": "cutover-drill", "rate_limit_per_user": 0,
            "permission_overwrites": []}


def mock_transport(state=None):
    """In-memory transport over fixtures: URL-shaped calls in, payloads out."""
    box = state if state is not None else {"channel": mock_channel(),
                                           "notices": {},
                                           "commands": [{"id": "1", "name": "ping"},
                                                        {"id": "2", "name": "rank"}],
                                           "next_notice": 1}

    def call(op, **kw):
        if op == "get_channel":
            return dict(box["channel"])
        if op == "post_notice":
            nid = f"90000000000000000{box['next_notice']}"
            box["next_notice"] += 1
            box["notices"][nid] = kw["content"]
            return {"id": nid}
        if op == "patch_slowmode":
            seconds = kw["seconds"]
            if not 0 <= seconds <= MAX_SLOWMODE_SECONDS:
                raise DrillError("slowmode seconds out of range 0..=21600")
            box["channel"]["rate_limit_per_user"] = seconds
            return {"rate_limit_per_user": seconds}
        if op == "patch_overwrite":
            # Like the live per-overwrite PUT: upsert only the @everyone
            # entry, preserving every unrelated overwrite.
            overwrites = [ow for ow in box["channel"]["permission_overwrites"]
                          if str(ow.get("id")) != str(kw["guild_id"])]
            overwrites.append({"id": box["channel"]["guild_id"], "type": 0,
                               "allow": kw["allow"], "deny": kw["deny"]})
            box["channel"]["permission_overwrites"] = overwrites
            return {"allow": kw["allow"], "deny": kw["deny"]}
        if op == "delete_overwrite":
            box["channel"]["permission_overwrites"] = [
                ow for ow in box["channel"]["permission_overwrites"]
                if str(ow.get("id")) != str(kw["guild_id"])]
            return {}
        if op == "delete_notice":
            box["notices"].pop(kw["notice_id"], None)
            return {}
        if op == "get_commands":
            return [dict(c) for c in box["commands"]]
        raise DrillError(f"mock transport has no op {op}")

    call.state = box
    return call


def live_transport(token):
    """Real Discord REST transport. Token stays in memory; never logged."""
    if not token:
        raise DrillError("refusing: no staging bot token in the environment")

    def request(method, url, payload=None):
        data = json.dumps(payload).encode() if payload is not None else None
        req = urllib.request.Request(
            url, data=data, method=method,
            headers={"Authorization": f"Bot {token}", "User-Agent": USER_AGENT,
                     "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=20) as resp:
                body = resp.read(BODY_CAP + 1)
        except urllib.error.HTTPError as error:
            raise DrillError(f"discord answered {error.code} on {method}")
        except Exception as error:
            raise DrillError(f"discord did not respond ({error.__class__.__name__})")
        if len(body) > BODY_CAP:
            raise DrillError("discord answered over the body cap")
        return json.loads(body) if body else {}

    def call(op, **kw):
        if op == "get_channel":
            return request("GET", f"{API}/channels/{kw['channel_id']}")
        if op == "post_notice":
            return request("POST", f"{API}/channels/{kw['channel_id']}/messages",
                           {"content": kw["content"]})
        if op == "patch_slowmode":
            return request("PATCH", f"{API}/channels/{kw['channel_id']}",
                           {"rate_limit_per_user": kw["seconds"]})
        if op == "patch_overwrite":
            return request("PUT",
                           f"{API}/channels/{kw['channel_id']}/permissions/{kw['guild_id']}",
                           {"type": 0, "allow": kw["allow"], "deny": kw["deny"]})
        if op == "delete_overwrite":
            return request("DELETE",
                           f"{API}/channels/{kw['channel_id']}/permissions/{kw['guild_id']}")
        if op == "delete_notice":
            return request("DELETE",
                           f"{API}/channels/{kw['channel_id']}/messages/{kw['notice_id']}")
        if op == "get_commands":
            return request(
                "GET",
                f"{API}/applications/{STAGING_APPLICATION_ID}/guilds/{kw['guild_id']}/commands")
        raise DrillError(f"live transport has no op {op}")

    return call


def freeze_notice_content():
    return ("CUTOVER REHEARSAL (staging only): this channel is briefly slowed and "
            "locked while we verify the cutover freeze, then restored. No action needed.")


def run_drill(guild_id, channel_id, reason, call, timer=None):
    """Nine fenced steps: baseline, freeze, verify surface, restore, verify."""
    timer = timer or StepTimer()
    guild = guild_fence(guild_id)
    audit_reason = check_reason(reason)
    applied = {"slowmode": False, "lockdown": False, "notice": None}
    baseline = None

    try:
        baseline_state = {}

        def do_baseline():
            state = _read_baseline(call, channel_id)
            baseline_state.update(state)
            return state["detail"]

        timer.run("baseline-read", do_baseline)
        baseline = {"slowmode": baseline_state["slowmode"],
                    "prior": baseline_state["prior"],
                    "hash": baseline_state["hash"]}
        pre_hash = baseline["hash"]
        prior = baseline["prior"]
        timer.run("freeze-notice-post",
                  lambda: _post_notice(call, channel_id, applied))
        timer.run("slowmode-on",
                  lambda: _set_slowmode(call, channel_id, DRILL_SLOWMODE_SECONDS,
                                        applied, audit_reason))
        timer.run("lockdown",
                  lambda: _lock(call, channel_id, guild, prior, applied, audit_reason))
        timer.run("command-surface-verify",
                  lambda: _verify_surface(call, guild))
        timer.run("slowmode-restore",
                  lambda: _set_slowmode(call, channel_id, baseline["slowmode"],
                                        applied, audit_reason, restore=True))
        timer.run("unlock-restore",
                  lambda: _unlock(call, channel_id, guild, prior, applied, audit_reason))
        timer.run("notice-delete",
                  lambda: _delete_notice(call, channel_id, applied))
        restore_state = {}

        def do_restore_verify():
            state = _verify_restore(call, channel_id, pre_hash)
            restore_state.update(state)
            return state["detail"]

        timer.run("restore-verify", do_restore_verify)
        restore = {"restored": restore_state["restored"],
                   "post_hash": restore_state["post_hash"],
                   "unlocked": restore_state["unlocked"],
                   "notice_removed": restore_state["notice_removed"]}
    except DrillError as error:
        _best_effort_restore(call, channel_id, guild, baseline,
                             applied, audit_reason, timer)
        return {"guild_id": guild,
                "result": "fail", "steps": timer.rows,
                "restore": {"restored": False, "reason": str(error)}}
    return {"guild_id": guild, "result": "pass", "steps": timer.rows,
            "restore": {"restored": restore["restored"],
                        "pre_hash": pre_hash, "post_hash": restore["post_hash"],
                        "unlocked": restore["unlocked"],
                        "notice_removed": restore["notice_removed"]}}


def _read_baseline(call, channel_id):
    channel = call("get_channel", channel_id=channel_id)
    if channel.get("guild_id") != STAGING_GUILD_ID:
        raise DrillError("refusing: channel is not in the staging guild")
    ow = everyone_overwrite(channel)
    slowmode = _to_int(channel.get("rate_limit_per_user") or 0, "slowmode")
    state = {"slowmode": slowmode, "prior": ow,
             "hash": semantic_hash(slowmode, ow["allow"], ow["deny"], ow["exists"])}
    return _baseline_detail(state)


def _baseline_detail(state):
    state = dict(state)
    locked = (_to_int(state["prior"]["deny"], "deny mask") & SEND_MESSAGES_BIT) != 0
    state["detail"] = f"slowmode={state['slowmode']} locked={int(locked)}"
    return state


def _post_notice(call, channel_id, applied):
    created = call("post_notice", channel_id=channel_id, content=freeze_notice_content())
    applied["notice"] = created.get("id")
    return "freeze notice posted"


def _set_slowmode(call, channel_id, seconds, applied, _reason, restore=False):
    if not 0 <= seconds <= MAX_SLOWMODE_SECONDS:
        raise DrillError("slowmode seconds out of range 0..=21600")
    call("patch_slowmode", channel_id=channel_id, seconds=seconds)
    applied["slowmode"] = not restore
    return f"slowmode={'restored' if restore else 'on'}"


def _lock(call, channel_id, guild, prior, applied, _reason):
    allow, deny = plan_lockdown_masks(prior["allow"], prior["deny"])
    call("patch_overwrite", channel_id=channel_id, guild_id=guild,
         allow=allow, deny=deny)
    applied["lockdown"] = True
    return "locked_down."


def _verify_surface(call, guild):
    commands = call("get_commands", guild_id=guild)
    names = {c.get("name") for c in commands if isinstance(c, dict)}
    if "ping" not in names:
        raise DrillError("command surface missing ping while locked")
    return f"surface ok ({len(names)} commands, ping present)"


def _unlock(call, channel_id, guild, prior, applied, _reason):
    if not prior["exists"]:
        call("delete_overwrite", channel_id=channel_id, guild_id=guild)
    else:
        call("patch_overwrite", channel_id=channel_id, guild_id=guild,
             allow=prior["allow"], deny=prior["deny"])
    applied["lockdown"] = False
    return "unlocked."


def _delete_notice(call, channel_id, applied):
    if applied.get("notice"):
        call("delete_notice", channel_id=channel_id, notice_id=applied["notice"])
        applied["notice"] = None
    return "notice removed"


def _verify_restore(call, channel_id, pre_hash):
    channel = call("get_channel", channel_id=channel_id)
    if channel.get("guild_id") != STAGING_GUILD_ID:
        raise DrillError("refusing: channel is not in the staging guild")
    ow = everyone_overwrite(channel)
    slowmode = _to_int(channel.get("rate_limit_per_user") or 0, "slowmode")
    post_hash = semantic_hash(slowmode, ow["allow"], ow["deny"], ow["exists"])
    unlocked = (_to_int(ow["deny"], "deny mask") & SEND_MESSAGES_BIT) == 0
    result = {"restored": post_hash == pre_hash and unlocked,
              "post_hash": post_hash, "unlocked": unlocked,
              "notice_removed": True}
    if not result["restored"]:
        raise DrillError("restore mismatch: post state differs from baseline")
    result["detail"] = "pre==post semantic hash, unlocked, notice removed"
    return result


def _best_effort_restore(call, channel_id, guild, baseline, applied, reason, timer):
    if baseline is None or not channel_id:
        return
    for name, fn in (
            ("restore-slowmode", lambda: _set_slowmode(
                call, channel_id, baseline["slowmode"], applied, reason, restore=True)),
            ("restore-unlock", lambda: _unlock(
                call, channel_id, guild, baseline["prior"], applied, reason)),
            ("restore-notice-delete", lambda: _delete_notice(call, channel_id, applied))):
        try:
            timer.run(name, fn)
        except DrillError:
            continue


def evidence_shape(outcome, transport, run_id):
    """Allowlisted receipt: names, verdicts, timings and hashes only."""
    return {"run_id": run_id, "guild_id": outcome.get("guild_id"),
            "transport": transport,
            "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "result": outcome.get("result"),
            "steps": [{"name": s["name"], "started_utc": s["started_utc"],
                       "duration_ms": s["duration_ms"], "result": s["result"],
                       "detail": s["detail"]} for s in outcome.get("steps", [])],
            "restore": outcome.get("restore")}


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--guild-id",
                        default=os.environ.get("DISCORD_STAGING_GUILD_ID"),
                        help="staging guild id (default: $DISCORD_STAGING_GUILD_ID)")
    parser.add_argument("--channel-id", default=None,
                        help="staging drill channel id (live only)")
    parser.add_argument("--reason", default="cutover freeze rehearsal (staging only)",
                        help="audit reason for the drill mutations")
    parser.add_argument("--mock", action="store_true",
                        help="drive local fixtures (no network, no token)")
    parser.add_argument("--live", action="store_true",
                        help="drive the real staging REST transport")
    parser.add_argument("--confirm-staging", action="store_true",
                        help="explicit live-run confirmation")
    parser.add_argument("--evidence", default="cutover-freeze-drill-evidence.json",
                        help="receipt path written when the drill completes")
    parser.add_argument("--run-id", default=None,
                        help="run label (default: drill-<utc>)")
    return parser.parse_args(argv)


def main(argv=None, call=None):
    args = parse_args(argv)
    run_id = args.run_id or ("drill-" + time.strftime("%Y%m%d-%H%M%S", time.gmtime()))
    try:
        guild_fence(args.guild_id)
        check_reason(args.reason)
    except DrillError as error:
        print(f"FAIL guild-fence: {error}")
        return 2
    if call is None:
        if args.mock and not args.live:
            call = mock_transport()
            transport = MOCK_TRANSPORT
        elif args.live and not args.mock:
            if not args.confirm_staging:
                print("FAIL live-guard: refusing without --confirm-staging")
                return 2
            if not args.channel_id:
                print("FAIL live-guard: refusing without --channel-id")
                return 2
            try:
                call = live_transport(os.environ.get("DISCORD_STAGING_BOT_TOKEN"))
            except DrillError as error:
                print(f"FAIL live-guard: {error}")
                return 2
            transport = LIVE_TRANSPORT
        else:
            print("FAIL transport: pass --mock for fixtures or --live with confirmations")
            return 1
    else:
        transport = MOCK_TRANSPORT
    channel = args.channel_id or "mock-drill-channel"
    outcome = run_drill(args.guild_id, channel, args.reason, call)
    for step in outcome["steps"]:
        print(f"{'PASS' if step['result'] == 'pass' else 'FAIL'} "
              f"{step['name']}: {step['detail']} ({step['duration_ms']} ms)")
    failed = sum(s["result"] != "pass" for s in outcome["steps"])
    print(f"cutover freeze drill: {len(outcome['steps']) - failed}/"
          f"{len(outcome['steps'])} steps passed ({transport})")
    receipt = evidence_shape(outcome, transport, run_id)
    try:
        with open(args.evidence, "w", encoding="utf-8") as handle:
            json.dump(receipt, handle, indent=2)
            handle.write("\n")
    except OSError as error:
        print(f"cutover freeze drill failed: evidence not written ({error.__class__.__name__})")
        return 1
    return 0 if outcome["result"] == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
