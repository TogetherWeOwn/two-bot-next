#!/usr/bin/env python3
"""Live staging voice synthetic: a dedicated test bot joins the staging creator
and the harness asserts the temp-voice room lifecycle, so no human has to join.

What it proves on the deployed staging bot (stdlib only, no third-party code):

  create     joining the creator produces a new room in the creator's category
             and moves the test bot into it
  name       the room is CREATED already carrying a template name (not the
             "<display>'s room" fallback), so no rename is needed
  position   the room sits directly below its creator among the category's
             voice channels
  owner      the room has a member overwrite for the joiner that allows View,
             Connect and Manage Channels (owner grants)
  status     a voice-channel status line is set on the room
  delete     after the test bot leaves, the room is deleted within the grace
  rename     the creator is renamed before the join and restored after: the
             post-rename join must still move the test bot into a new room
             (SKIP while the test bot may not rename: its staging role needs
             Manage Channels once; SKIP on HTTP 429, Discord's 2-renames-per-
             10-minutes per-channel limit, so workflow retries do not fail)
  metrics    optional: /ops/metrics `two_bot_voice_names_total` counts the room
             as created_with_template (SKIP when no metrics token is configured)
  limit      the room's `user_limit` PATCHes to a probed value and back with a
             read-back after each write (FAIL on HTTP 403: the owner grant was
             lost after creation; SKIP on HTTP 429; the same Discord
             field `/limit` and `/unlimit` manage)
  privacy    the room's @everyone overwrite does not deny Connect (the
             public baseline: a Connect deny is the `/private` marker), the
             staging product bot has effective View Channel and Manage Roles
             on the room (the `/private` overwrite precondition), and no
             member overwrite grants Manage Roles (read-only)
  name_panel the room's name after the rename cycle renders from its template
             (not the "<display>'s room" fallback): what the `/name` panel's
             Current-name line would show

The limit check exercises the channel field `/limit` and `/unlimit` write;
privacy reads the public @everyone baseline (no Connect deny) and the
staging product bot's
computed View Channel and Manage Roles permission (the `/private`
precondition, mirroring `can_edit_overwrites`), and name_panel
reads the template-rendered name. These checks do not invoke slash commands
(Discord only creates interactions for real users, so a bot token cannot). A
PASS means the room the staging bot made has the expected owner permissions and
privacy baseline; a FAIL names the fact that did not hold.

Identity and fences (fail-closed, before any request):
  - TWO_VOICE_SYNTHETIC_BOT_TOKEN: a dedicated test bot (never the bot under
    test). The staging bot counts it as a human occupant only through the
    staging-only TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS setting.
  - Guild must be exactly TWO Staging; the live guild or anything else refuses
    with exit 2 and sends nothing.
  - Creator must be the staging "Create a Lobby" creator (the only creator).

Exit codes: 0 every check passed (or SKIP where allowed), 1 a check failed,
2 refused before contact (fence or missing configuration).

Usage:
  TWO_VOICE_SYNTHETIC_BOT_TOKEN=... python3 scripts/staging_voice_synthetic.py \\
      [--staging-url URL] [--evidence FILE]
"""

import argparse
import base64
import json
import math
import os
import re
import socket
import ssl
import struct
import sys
import threading
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit

STAGING_GUILD_ID = "1545644954272137297"
LIVE_GUILD_ID = "326474832151838730"
# Staging "Create a Lobby" creator (the only creator; "Join to Create" is retired).
STAGING_CREATOR_ID = "1546211378430345286"
STAGING_HOST = re.compile(r"two-bot-next-staging\.[a-z0-9-]+\.workers\.dev")
API = "https://discord.com/api/v10"
GATEWAY = "gateway.discord.gg"
INTENTS = 1 | (1 << 7)  # GUILDS | GUILD_VOICE_STATES (no privileged intents)
VIEW, CONNECT, MANAGE = 1 << 10, 1 << 20, 1 << 4
MANAGE_ROLES, ADMINISTRATOR = 1 << 28, 1 << 3
OWNER_BITS = VIEW | CONNECT | MANAGE
STAGING_PRODUCT_BOT_ID = "1469137636663758888"
VOICE_TYPES = (2, 13)
FALLBACK = re.compile(r"^.+['’]s room$")
# Marker appended to the staging creator's name for the rename case. The run
# restores the original name in a finally block; a name already carrying the
# marker means a previous run did not restore, and the rename is skipped.
RENAME_MARKER = " (synthetic)"

# Fixed vocabulary for an attempted user_limit write without a verified
# read-back (lost/ambiguous probe response, failed read, mismatch, or a
# non-2xx status other than 403/429). The limit check FAILs with this reason
# even when the bounded cleanup restores the original value, so an uncertain
# write never reads as green. A verified cleanup names the room, the original
# value and `re-run`; a failed/unverified cleanup names the room, the
# original value, `restore` and `manually`.
LIMIT_UNCERTAIN = "room user_limit write was not verified"

# Body-cap refusal contract (same shape as staging_probe.py MAX_BODY_BYTES and
# the contract probe BODY_CAP): every response read takes at most
# MAX_BODY_BYTES+1 bytes so a hidden suffix is detected. Any body longer than
# MAX_BODY_BYTES (64 KiB) is refused before decoding/parsing with a fixed
# message; exact-cap valid input passes, and the extra byte never reaches a
# successful JSON path. Refusals never echo the body, headers, URL or parse
# exceptions.
MAX_BODY_BYTES = 64 * 1024
BODY_CAP = MAX_BODY_BYTES

# Timing-input bound (seconds). Every synthetic deadline is a live gateway
# wait, and the workflow budget is one run: voice-synthetic.yml runs this
# script with timeout-minutes: 10, so 600 s is the largest meaningful value.
# A larger deadline only probes a run the workflow would kill first, and a
# non-finite one (NaN/inf) would hang or skip the wait silently. All four
# timing inputs (flag and env-var sourced) go through timing_seconds, which
# refuses anything outside (0, TIMING_CAP_SECONDS] before any live call.
TIMING_CAP_SECONDS = 600.0
TIMING_REFUSAL = ("timing input must be a finite number of seconds above 0 "
                  f"and at most {TIMING_CAP_SECONDS:g}")


def timing_seconds(raw):
    """One finite, positive, bounded argparse type for the timing inputs."""
    try:
        value = float(raw)
    except (TypeError, ValueError):
        raise argparse.ArgumentTypeError(TIMING_REFUSAL) from None
    if not math.isfinite(value) or not 0 < value <= TIMING_CAP_SECONDS:
        raise argparse.ArgumentTypeError(TIMING_REFUSAL)
    return value


class Refusal(Exception):
    pass


class BodyTooLarge(ValueError):
    """A response body exceeds MAX_BODY_BYTES (fixed vocabulary only)."""


REDIRECT_STATUSES = (301, 302, 303, 307, 308)


class RedirectRefused(ValueError):
    """An authenticated read answered a redirect (fixed vocabulary only)."""


class DuplicateKeyError(ValueError):
    """A JSON object repeats a key; the message never carries key/value data."""


def _reject_duplicate_keys(pairs):
    """object_pairs_hook refusing repeats instead of last-wins.

    The json decoder calls this for every object at any depth, after
    escape decoding, so {"a": 1, "\\u0061": 2} counts as a repeat. The
    raised message is fixed vocabulary: it names no key, value or path.
    """
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise DuplicateKeyError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


class GatewayShapeError(ValueError):
    """A gateway frame, hello or dispatch violates the expected shape.

    Fixed vocabulary only, like BodyTooLarge and RedirectRefused.
    """


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Turn every redirect into an HTTPError without contacting its target."""

    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


def _open_no_redirect(req, timeout):
    """One token-bearing request that never follows a redirect.

    urllib's default redirect handler would replay the Authorization header
    to the redirect target; rejecting redirects at the opener means only the
    intended URL is ever contacted. A 3xx surfaces as HTTPError so each
    reader reports its fixed-vocabulary redirect refusal.
    """
    opener = urllib.request.build_opener(
        urllib.request.HTTPHandler(),
        urllib.request.HTTPSHandler(),
        urllib.request.HTTPErrorProcessor(),
        _NoRedirectHandler(),
    )
    return opener.open(req, timeout=timeout)


# --------------------------------------------------------------------------
# Pure verdict logic (offline-testable)
# --------------------------------------------------------------------------

def check(name, ok, detail, skip=False):
    return {"check": name, "verdict": "SKIP" if skip else ("PASS" if ok else "FAIL"), "detail": detail}


def voice_order(channels, parent_id):
    """Voice channels of one category in Discord's display order (position, id)."""
    rows = [c for c in channels if c.get("parent_id") == parent_id and c.get("type") in VOICE_TYPES]
    return sorted(rows, key=lambda c: (c.get("position", 0), int(c["id"])))


def effective_permissions(guild_id, member_id, member_role_ids, roles, overwrites,
                          guild_owner_id=None):
    """Compute Discord guild and channel permissions for one member."""
    if member_id == guild_owner_id:
        return (1 << 64) - 1
    role_ids = {str(role_id) for role_id in member_role_ids}
    role_by_id = {str(role.get("id")): role for role in roles}
    everyone = role_by_id.get(str(guild_id), {})
    permissions = int(everyone.get("permissions") or 0)
    for role_id in role_ids:
        permissions |= int(role_by_id.get(role_id, {}).get("permissions") or 0)
    if permissions & ADMINISTRATOR:
        return (1 << 64) - 1

    everyone_overwrite = next((o for o in overwrites
                               if int(o.get("type", -1)) == 0
                               and str(o.get("id")) == str(guild_id)), None)
    if everyone_overwrite:
        permissions &= ~int(everyone_overwrite.get("deny") or 0)
        permissions |= int(everyone_overwrite.get("allow") or 0)

    role_denies = role_allows = 0
    for overwrite in overwrites:
        if int(overwrite.get("type", -1)) == 0 and str(overwrite.get("id")) in role_ids:
            role_denies |= int(overwrite.get("deny") or 0)
            role_allows |= int(overwrite.get("allow") or 0)
    permissions &= ~role_denies
    permissions |= role_allows

    member_overwrite = next((o for o in overwrites
                             if int(o.get("type", -1)) == 1
                             and str(o.get("id")) == str(member_id)), None)
    if member_overwrite:
        permissions &= ~int(member_overwrite.get("deny") or 0)
        permissions |= int(member_overwrite.get("allow") or 0)
    return permissions


def privacy_facts(channel, roles, member, guild_id, product_bot_id, guild_owner_id=None):
    """Return the three read-only privacy assertions for the newly created room."""
    overwrites = channel.get("permission_overwrites", []) or []
    everyone = [o for o in overwrites
                if int(o.get("type", -1)) == 0 and str(o.get("id")) == str(guild_id)]
    everyone_allow = int(everyone[0].get("allow") or 0) if len(everyone) == 1 else 0
    everyone_deny = int(everyone[0].get("deny") or 0) if len(everyone) == 1 else 0
    # Public is the absence of the private marker: `/private` denies Connect
    # to @everyone and `/public` only removes that deny
    # (everyone_connect_overwrite in
    # crates/bot/src/voice_rooms/private_runtime.rs), while a new room
    # inherits its source channel's overwrites, which commonly allow
    # Connect explicitly. An allow therefore stays public; only a deny
    # (or a malformed duplicate @everyone entry) breaks the baseline.
    public_baseline = len(everyone) <= 1 and not (everyone_deny & CONNECT)
    no_member_grant = not any(
        int(o.get("type", -1)) == 1 and int(o.get("allow") or 0) & MANAGE_ROLES
        for o in overwrites)
    product_member_roles = [str(role_id) for role_id in member.get("roles", [])]
    product_permissions = effective_permissions(
        guild_id, product_bot_id, product_member_roles, roles, overwrites,
        guild_owner_id=guild_owner_id)
    # Mirrors can_edit_overwrites in
    # crates/bot/src/voice_rooms/private_runtime.rs: rewriting the @everyone
    # overwrite needs VIEW_CHANNEL|MANAGE_ROLES, not Manage Roles alone.
    product_can_edit = (product_permissions & (VIEW | MANAGE_ROLES)) == (VIEW | MANAGE_ROLES)
    role_by_id = {str(role.get("id")): role for role in roles}
    product_role_names = [role_by_id[role_id].get("name", role_id)
                          for role_id in product_member_roles if role_id in role_by_id]
    product_role = ", ".join(product_role_names) or "no assigned staging role"
    return {
        "privacy_public_baseline": public_baseline,
        "privacy_everyone_allow": everyone_allow,
        "privacy_everyone_deny": everyone_deny,
        "privacy_everyone_count": len(everyone),
        "privacy_product_bot_manage_roles": bool(product_can_edit),
        "privacy_no_member_manage_roles": no_member_grant,
        "privacy_product_bot_role": product_role,
    }


def creator_block(channels_before, creator):
    """The creator's existing rooms before the join, mirroring plan_placement:
    a creator that already has rooms groups the new one at the end of that
    block. A room is a voice channel carrying a member (type 1) overwrite that
    grants Manage Channels (the owner grant); the block is the run of such
    channels directly after the creator."""
    order = voice_order(channels_before, creator.get("parent_id"))
    ids = [c["id"] for c in order]
    if creator["id"] not in ids:
        return []
    block = []
    for channel in order[ids.index(creator["id"]) + 1:]:
        owned = any(o.get("type") == 1 and int(o.get("allow", 0)) & MANAGE
                    for o in channel.get("permission_overwrites", []))
        if not owned:
            break
        block.append(channel["id"])
    return block


def evaluate(obs):
    """Checks for one synthetic run. ``obs`` keys:

    me, creator (channel dict), room_id, room_created (CHANNEL_CREATE payload or
    None), moved_to (the channel the joiner's own voice state landed in after
    joining the creator), rejoin_room_id (the room created when the joiner hopped
    from their room back into the creator, or None), channels (guild channels
    after the move), statuses (room status
    strings seen), deleted (bool), delete_seconds, grace_seconds, metrics_delta
    (int or None when unavailable), channels_before (guild channels before the
    join; the creator's existing rooms move the expected slot to their end),
    received_delta (joins-received delta or None when metrics are unavailable),
    refused_delta (per-reason refused deltas, {} when unavailable),
    limit_initial/limit_probed (user_limit before/probed), limit_set_ok and
    limit_restored (each write verified by read-back), limit_forbidden (FAIL
    reason: the owner Manage Channels grant was lost after creation) and
    limit_rate_limited (SKIP reason), limit_note, privacy_public_baseline,
    privacy_everyone_allow/deny/count (the raw @everyone facts behind the
    baseline), privacy_product_bot_manage_roles, privacy_no_member_manage_roles,
    privacy_product_bot_role (read-only privacy facts), privacy_note, panel_name
    (the room's name read after the rename cycle) and panel_note.
    """
    out = []
    creator, room_id = obs["creator"], obs.get("room_id")
    created = obs.get("room_created") or {}
    out.append(check("create", bool(room_id) and created.get("parent_id") == creator.get("parent_id"),
                     f"room={room_id} parent={created.get('parent_id')} creator_parent={creator.get('parent_id')}"
                     f"{refusal_suffix(obs)}"))
    out.append(check("moved", bool(room_id) and obs.get("moved_to") == room_id,
                     f"joiner landed in {obs.get('moved_to')} (room {room_id})"))
    rejoin = obs.get("rejoin_room_id")
    out.append(check("rejoin", bool(rejoin) and rejoin not in (room_id, creator.get("id")),
                     f"hop from room {room_id} back into the creator -> {rejoin} (None = left in the lobby)"))
    name = created.get("name", "")
    out.append(check("name", bool(name) and not FALLBACK.match(name),
                     f"created name {name!r} (fallback pattern <display>'s room must not match)"))
    order = [c["id"] for c in voice_order(obs.get("channels", []), creator.get("parent_id"))]
    live = set(order)
    before_block = [r for r in creator_block(obs.get("channels_before", []), creator) if r != room_id]
    # A snapshotted predecessor the empty-room sweep deleted mid-run is not a
    # misplacement: the expected slot follows the predecessors still present
    # when the room's position is read, and the detail names the swept ones.
    block = [r for r in before_block if r in live]
    after = block[-1] if block else creator["id"]
    ok = room_id in order and after in order and order.index(room_id) == order.index(after) + 1
    detail = (f"category voice order {order}; room must follow {after} "
              f"(creator {creator['id']}, its existing rooms {block})")
    swept = [r for r in before_block if r not in live]
    if swept:
        detail += f"; swept since the snapshot: {swept}"
    out.append(check("position", ok, detail))
    mine = [o for o in created.get("permission_overwrites", [])
            if o.get("type") == 1 and o.get("id") == obs["me"]]
    allow = int(mine[0]["allow"]) if mine else 0
    out.append(check("owner", allow & OWNER_BITS == OWNER_BITS,
                     f"joiner overwrite allow={allow} needs View|Connect|Manage Channels"))
    statuses = [s for s in obs.get("statuses", []) if s]
    out.append(check("status", bool(statuses), f"status lines seen: {statuses[:3]}"))
    out.append(check("delete", bool(obs.get("deleted")),
                     f"deleted={obs.get('deleted')} after {obs.get('delete_seconds')}s (grace {obs.get('grace_seconds')}s)"))
    delta = obs.get("metrics_delta")
    out.append(check("metrics", delta is not None and delta >= 1,
                     "no metrics token configured" if delta is None else f"created_with_template +{delta}",
                     skip=delta is None))
    note = obs.get("rename_note")
    if obs.get("rename_forbidden"):
        out.append(check("rename", False,
                         "test bot may not rename the staging creator (HTTP 403): grant its "
                         "staging role Manage Channels once, then the rename case gates",
                         skip=True))
    elif obs.get("rename_rate_limited"):
        out.append(check("rename", False,
                         "creator rename hit Discord's per-channel rename limit (HTTP 429): "
                         "a retry or dispatch collided within 10 minutes; rename skipped",
                         skip=True))
    elif obs.get("rename_applied") and obs.get("rename_restored"):
        out.append(check("rename", True,
                         f"creator renamed to {obs.get('rename_name')!r} and restored; "
                         f"the post-rename join landed in {obs.get('moved_to')}"))
    elif note and not obs.get("rename_applied"):
        out.append(check("rename", False, note))
    elif obs.get("rename_applied") and not obs.get("rename_restored"):
        out.append(check("rename", False,
                         "creator was renamed but the original name was NOT restored; "
                         "restore it manually"))
    else:
        out.append(check("rename", False, note or "creator rename was not applied"))
    limit_note = obs.get("limit_note")
    if obs.get("limit_forbidden"):
        out.append(check("limit", False,
                         "room user_limit PATCH answered HTTP 403: the test bot owns "
                         "its room, so its owner Manage Channels grant was lost or "
                         "ineffective after creation; restore the member overwrite "
                         "(the owner check reads creation-time grants and cannot "
                         "see this loss)"))
    elif obs.get("limit_rate_limited"):
        out.append(check("limit", False,
                         "room user_limit PATCH hit Discord's rate limit (HTTP 429): "
                         "a retry or dispatch collided; limit skipped",
                         skip=True))
    elif obs.get("limit_set_ok") and obs.get("limit_restored"):
        out.append(check("limit", True,
                         f"user_limit {obs.get('limit_initial')} -> {obs.get('limit_probed')} "
                         f"-> {obs.get('limit_initial')} (each write verified by read-back)"))
    elif limit_note and not obs.get("limit_set_ok"):
        out.append(check("limit", False, limit_note))
    elif obs.get("limit_set_ok") and not obs.get("limit_restored"):
        out.append(check("limit", False,
                         "room user_limit was set but NOT restored to "
                         f"{obs.get('limit_initial')}; restore it manually"))
    else:
        out.append(check("limit", False, limit_note or "room user_limit was not round-tripped"))
    privacy_note = obs.get("privacy_note")
    public_baseline = obs.get("privacy_public_baseline", False)
    product_manage_roles = obs.get("privacy_product_bot_manage_roles", False)
    no_member_grant = obs.get("privacy_no_member_manage_roles", False)
    product_role = obs.get("privacy_product_bot_role") or "no assigned staging role"
    privacy_detail = (
        f"@everyone public Connect baseline={'PASS' if public_baseline else 'FAIL'} "
        f"(allow={obs.get('privacy_everyone_allow')} deny={obs.get('privacy_everyone_deny')} "
        f"entries={obs.get('privacy_everyone_count')}); "
        f"product bot staging role {product_role!r} effective View Channel|Manage Roles="
        f"{'PASS' if product_manage_roles else 'FAIL'}; "
        f"member overwrites grant Manage Roles={'no' if no_member_grant else 'yes'}"
    )
    if privacy_note:
        out.append(check("privacy", False, privacy_note))
    else:
        out.append(check("privacy", public_baseline and product_manage_roles and no_member_grant,
                         privacy_detail))
    panel = obs.get("panel_name") or ""
    if panel and not FALLBACK.match(panel):
        out.append(check("name_panel", True,
                         f"post-rename room name {panel!r} renders from its template "
                         "(what the /name panel's Current-name line shows)"))
    else:
        out.append(check("name_panel", False,
                         obs.get("panel_note") or
                         f"post-rename room name {panel!r} is missing or the fallback"))
    return out


def refusal_suffix(obs):
    """Name the refusal reason on a failed create: joins-received plus per-reason
    refused deltas when metrics are available, or a metrics-unavailable note.
    The next restart-cured stall must name its reason, not just room=None."""
    refused = obs.get("refused_delta") or {}
    total = sum(refused.values())
    received = obs.get("received_delta")
    if received is None and not refused:
        return ""
    parts = []
    if received is not None:
        parts.append(f"received={received:g}")
    for reason in sorted(refused):
        if refused[reason]:
            parts.append(f"{reason}={refused[reason]:g}")
    if not parts:
        if received is not None:
            return f"; joins received={received:g}, no refusals"
        return ""
    return "; joins " + ", ".join(parts)


def fence(guild_id, creator_id, staging_url):
    if guild_id == LIVE_GUILD_ID or guild_id != STAGING_GUILD_ID:
        raise Refusal(f"guild {guild_id!r} is not TWO Staging; refusing before any request")
    if creator_id != STAGING_CREATOR_ID:
        raise Refusal(f"creator {creator_id!r} is not the staging Create a Lobby creator")
    if staging_url:
        try:
            parts = urlsplit(staging_url)
        except ValueError:
            raise Refusal("staging URL must be the two-bot-next-staging workers.dev origin")
        try:
            port = parts.port
        except ValueError:
            raise Refusal("staging URL must be the two-bot-next-staging workers.dev origin")
        if (parts.scheme != "https" or port is not None
                or parts.username is not None or parts.password is not None
                or not STAGING_HOST.fullmatch(parts.hostname or "")
                or parts.path not in ("", "/") or parts.query or parts.fragment):
            raise Refusal("staging URL must be the two-bot-next-staging workers.dev origin")


def counter(text, outcome):
    """Sum of two_bot_voice_names_total{outcome=...} samples in a Prometheus body."""
    total = 0.0
    for line in text.splitlines():
        if line.startswith("two_bot_voice_names_total{") and f'outcome="{outcome}"' in line:
            try:
                total += float(line.rsplit(" ", 1)[1])
            except ValueError:
                pass
    return total


def parse_received(text):
    """Value of two_bot_voice_joins_received_total in a Prometheus body (0.0 when absent)."""
    for line in text.splitlines():
        if line.startswith("two_bot_voice_joins_received_total "):
            try:
                return float(line.rsplit(" ", 1)[1])
            except ValueError:
                pass
    return 0.0


def parse_refusals(text):
    """Per-reason two_bot_voice_join_refused_total{reason=...} counts in a Prometheus body."""
    out = {}
    for line in text.splitlines():
        if not line.startswith("two_bot_voice_join_refused_total{"):
            continue
        try:
            value = float(line.rsplit(" ", 1)[1])
        except ValueError:
            continue
        for part in line.split("{", 1)[1].split("}", 1)[0].split(","):
            key, _, val = part.partition("=")
            if key.strip() == "reason":
                out[val.strip().strip('"')] = out.get(val.strip().strip('"'), 0.0) + value
    return out


def metrics_snapshot(staging_url, token):
    """One /ops/metrics fetch returning (received, refusals) or (None, None).

    `received` is the joins-received counter, `refusals` maps each refusal
    reason to its count. Unavailable metrics (no URL/token, HTTP error,
    network failure) report (None, None) so the caller SKIPs the refusal
    naming instead of guessing; an over-cap body raises BodyTooLarge like the
    other readers.
    """
    if not (staging_url and token):
        return None, None
    req = urllib.request.Request(f"{staging_url.rstrip('/')}/ops/metrics", headers={
        "Authorization": f"Bearer {token}", "User-Agent": "two-bot-voice-synthetic (staging)"})
    try:
        with _open_no_redirect(req, timeout=20) as resp:
            raw = resp.read(MAX_BODY_BYTES + 1)
    except urllib.error.HTTPError as exc:
        if exc.code in REDIRECT_STATUSES:
            raise RedirectRefused(
                f"metrics read refused redirect with status {exc.code}") from None
        raw = exc.read(MAX_BODY_BYTES + 1)
        if len(raw) > MAX_BODY_BYTES:
            raise BodyTooLarge("metrics read answered over the body cap") from None
        return None, None
    except (urllib.error.URLError, OSError):
        return None, None
    if len(raw) > MAX_BODY_BYTES:
        raise BodyTooLarge("metrics read answered over the body cap")
    text = raw.decode()
    return parse_received(text), parse_refusals(text)


# --------------------------------------------------------------------------
# Minimal RFC 6455 client (text frames, fragmentation, ping/pong, close)
# --------------------------------------------------------------------------

class WebSocket:
    def __init__(self, host, path, timeout=30):
        raw = socket.create_connection((host, 443), timeout=timeout)
        context = ssl.create_default_context()
        context.minimum_version = ssl.TLSVersion.TLSv1_2
        self.sock = context.wrap_socket(raw, server_hostname=host)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall((f"GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n"
                           f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
                           "Sec-WebSocket-Version: 13\r\n\r\n").encode())
        # Suffix-checked and byte-capped: the terminator can only complete at
        # the end of a byte-at-a-time read, so endswith keeps a hostile
        # header linear instead of rescanning the whole head per byte.
        head = bytearray()
        while not head.endswith(b"\r\n\r\n"):
            chunk = self.sock.recv(1)
            if not chunk:
                raise ConnectionError("gateway closed during handshake")
            head += chunk
            if len(head) > MAX_BODY_BYTES:
                raise BodyTooLarge("gateway handshake answered over the byte cap")
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            raise ConnectionError("gateway refused the websocket upgrade")
        self.lock = threading.Lock()

    def _exact(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("gateway closed")
            buf += chunk
        return buf

    def send(self, payload):
        data = json.dumps(payload).encode()
        mask = os.urandom(4)
        n = len(data)
        head = bytes([0x81])
        if n < 126:
            head += bytes([0x80 | n])
        elif n < 65536:
            head += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            head += bytes([0x80 | 127]) + struct.pack(">Q", n)
        body = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        with self.lock:
            self.sock.sendall(head + mask + body)

    def recv(self):
        """Next complete text message as JSON (answers pings; None on close).

        Every frame payload and every reassembled message is capped at
        MAX_BODY_BYTES, like the REST body cap: the declared length is
        validated before the payload is read, so a huge length refuses
        without an unbounded read, and continuation fragments refuse once
        their running total passes the cap. Exact-cap input passes.
        Control frames (ping, pong, close) carry at most 125 payload bytes
        per RFC 6455 section 5.5; a longer control payload refuses as a
        malformed frame instead of corrupting the pong encoder.
        """
        parts = []
        total = 0
        while True:
            b1, b2 = self._exact(2)
            op, fin, n = b1 & 0x0F, b1 & 0x80, b2 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self._exact(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self._exact(8))[0]
            if n > MAX_BODY_BYTES:
                raise BodyTooLarge("gateway frame answered over the byte cap")
            if op & 0x8 and n > 125:
                raise GatewayShapeError("gateway frame was malformed")
            if op in (0x1, 0x0) and total + n > MAX_BODY_BYTES:
                raise BodyTooLarge("gateway message answered over the byte cap")
            mask = self._exact(4) if b2 & 0x80 else None
            data = self._exact(n)
            if mask:
                data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
            if op == 0x8:
                return None
            if op == 0x9:
                with self.lock:
                    m = os.urandom(4)
                    self.sock.sendall(bytes([0x8A, 0x80 | len(data)]) + m
                                      + bytes(b ^ m[i % 4] for i, b in enumerate(data)))
                continue
            if op in (0x1, 0x0):
                total += len(data)
                if total > MAX_BODY_BYTES:
                    raise BodyTooLarge("gateway message answered over the byte cap")
                parts.append(data)
                if fin:
                    try:
                        return json.loads(b"".join(parts),
                                          object_pairs_hook=_reject_duplicate_keys)
                    except DuplicateKeyError:
                        raise ValueError(
                            "gateway frame carries duplicate JSON object keys") from None

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


# --------------------------------------------------------------------------
# Live run
# --------------------------------------------------------------------------

def rest(token, path, timeout=20):
    req = urllib.request.Request(f"{API}{path}", headers={
        "Authorization": f"Bot {token}", "User-Agent": "two-bot-voice-synthetic (staging)"})
    try:
        with _open_no_redirect(req, timeout=timeout) as resp:
            raw = resp.read(MAX_BODY_BYTES + 1)
    except urllib.error.HTTPError as exc:
        if exc.code in REDIRECT_STATUSES:
            raise RedirectRefused(
                f"Discord REST read refused redirect with status {exc.code}") from None
        raw = exc.read(MAX_BODY_BYTES + 1)
        if len(raw) > MAX_BODY_BYTES:
            raise BodyTooLarge("Discord REST read answered over the body cap") from None
        raise
    if len(raw) > MAX_BODY_BYTES:
        raise BodyTooLarge("Discord REST read answered over the body cap")
    try:
        return json.loads(raw, object_pairs_hook=_reject_duplicate_keys)
    except DuplicateKeyError:
        raise ValueError("Discord REST read carries duplicate JSON object keys") from None


def rest_patch(token, path, payload, timeout=20):
    """PATCH a Discord resource with a JSON body under the same body cap.

    Returns (status, parsed body or None). HTTP errors report their status
    instead of raising (a 403 means the test bot may not rename); only an
    over-cap body raises BodyTooLarge, like the GET path.
    """
    data = json.dumps(payload).encode()
    req = urllib.request.Request(f"{API}{path}", data=data, headers={
        "Authorization": f"Bot {token}", "User-Agent": "two-bot-voice-synthetic (staging)",
        "Content-Type": "application/json"}, method="PATCH")
    try:
        with _open_no_redirect(req, timeout=timeout) as resp:
            raw = resp.read(MAX_BODY_BYTES + 1)
            status = resp.status
    except urllib.error.HTTPError as exc:
        if exc.code in REDIRECT_STATUSES:
            raise RedirectRefused(
                f"Discord REST read refused redirect with status {exc.code}") from None
        raw = exc.read(MAX_BODY_BYTES + 1)
        if len(raw) > MAX_BODY_BYTES:
            raise BodyTooLarge("Discord REST read answered over the body cap") from None
        return exc.code, None
    if len(raw) > MAX_BODY_BYTES:
        raise BodyTooLarge("Discord REST read answered over the body cap")
    try:
        return status, json.loads(raw, object_pairs_hook=_reject_duplicate_keys)
    except DuplicateKeyError:
        raise ValueError("Discord REST read carries duplicate JSON object keys") from None


def metrics_value(staging_url, token):
    if not (staging_url and token):
        return None
    req = urllib.request.Request(f"{staging_url.rstrip('/')}/ops/metrics", headers={
        "Authorization": f"Bearer {token}", "User-Agent": "two-bot-voice-synthetic (staging)"})
    try:
        with _open_no_redirect(req, timeout=20) as resp:
            raw = resp.read(MAX_BODY_BYTES + 1)
    except urllib.error.HTTPError as exc:
        if exc.code in REDIRECT_STATUSES:
            raise RedirectRefused(
                f"metrics read refused redirect with status {exc.code}") from None
        raw = exc.read(MAX_BODY_BYTES + 1)
        if len(raw) > MAX_BODY_BYTES:
            raise BodyTooLarge("metrics read answered over the body cap") from None
        return None
    except (urllib.error.URLError, OSError):
        return None
    if len(raw) > MAX_BODY_BYTES:
        raise BodyTooLarge("metrics read answered over the body cap")
    return counter(raw.decode(), "created_with_template")


class Session:
    """Gateway session: identify, heartbeat, collect the events the checks need."""

    def __init__(self, token):
        self.token, self.events, self.me = token, [], None
        self.ws = WebSocket(GATEWAY, "/?v=10&encoding=json")
        hello = self.ws.recv()
        data = hello.get("d") if isinstance(hello, dict) else None
        interval = data.get("heartbeat_interval") if isinstance(data, dict) else None
        if isinstance(interval, bool) or not isinstance(interval, (int, float)):
            raise GatewayShapeError("gateway hello was malformed")
        try:
            seconds = interval / 1000
        except OverflowError:
            raise GatewayShapeError("gateway hello was malformed") from None
        if not math.isfinite(seconds) or not 0 < seconds <= TIMING_CAP_SECONDS:
            raise GatewayShapeError("gateway hello was malformed")
        self.interval = seconds
        self.seq, self.alive = None, True
        threading.Thread(target=self._beat, daemon=True).start()
        self.ws.send({"op": 2, "d": {"token": token, "intents": INTENTS,
                                     "properties": {"os": "linux", "browser": "voice-synthetic",
                                                    "device": "voice-synthetic"}}})

    def _beat(self):
        while self.alive:
            time.sleep(self.interval)
            try:
                self.ws.send({"op": 1, "d": self.seq})
            except OSError:
                return

    def pump(self, until, deadline):
        """Read dispatches until ``until(event)`` is true or the deadline passes."""
        while time.monotonic() < deadline:
            self.ws.sock.settimeout(max(0.5, deadline - time.monotonic()))
            try:
                msg = self.ws.recv()
            except socket.timeout:
                return None
            if msg is None:
                raise ConnectionError("gateway closed the session")
            if not isinstance(msg, dict):
                raise GatewayShapeError("gateway dispatch was malformed")
            if msg.get("s") is not None:
                self.seq = msg["s"]
            if msg.get("op") != 0:
                continue
            if not isinstance(msg.get("t"), str) or not isinstance(msg.get("d"), dict):
                raise GatewayShapeError("gateway dispatch was malformed")
            if msg["t"] in ("GUILD_CREATE", "CHANNEL_DELETE"):
                if not isinstance(msg["d"].get("id"), str):
                    raise GatewayShapeError("gateway dispatch was malformed")
            event = {"t": msg["t"], "d": msg["d"]}
            self.events.append(event)
            if msg["t"] == "READY":
                user = msg["d"].get("user")
                if not isinstance(user, dict) or not isinstance(user.get("id"), str):
                    raise GatewayShapeError("gateway dispatch was malformed")
                self.me = user["id"]
            if until(event):
                return event
        return None

    def voice(self, guild_id, channel_id):
        self.ws.send({"op": 4, "d": {"guild_id": guild_id, "channel_id": channel_id,
                                     "self_mute": True, "self_deaf": True}})

    def close(self):
        self.alive = False
        self.ws.close()


def run_live(args, token):
    obs = {"grace_seconds": args.grace, "statuses": [], "deleted": False,
           "received_delta": None, "refused_delta": {}}
    before = metrics_value(args.staging_url, os.environ.get("TWO_STAGING_METRICS_TOKEN"))
    snap_before = metrics_snapshot(args.staging_url, os.environ.get("TWO_STAGING_METRICS_TOKEN"))
    s = Session(token)
    joined = False
    try:
        if not s.pump(lambda e: e["t"] == "GUILD_CREATE" and e["d"]["id"] == args.guild, time.monotonic() + 30):
            raise RuntimeError("the test bot is not in the staging guild (no GUILD_CREATE)")
        obs["me"] = s.me
        creator = rest(token, f"/channels/{args.creator}")
        obs["creator"] = creator
        # Rename case: a renamed creator must still move joins into new rooms
        # (a stale creator view refused them until restart). The original name
        # is restored in the finally block; residue from a previous run skips
        # the rename but still runs the lifecycle checks.
        original_name = creator.get("name", "")
        obs["rename_original"] = original_name
        obs["rename_applied"] = obs["rename_restored"] = False
        obs["rename_forbidden"] = obs["rename_rate_limited"] = False
        # Set before the PATCH: a request Discord applies but whose response
        # is lost (timeout, failed read-back) must still be restored.
        obs["rename_attempted"] = False
        temp_name = (original_name + RENAME_MARKER)[:100] or "Create a Lobby" + RENAME_MARKER
        if RENAME_MARKER in original_name:
            obs["rename_note"] = ("creator already carries synthetic residue "
                                  f"({original_name!r}); rename skipped, restore it manually")
        else:
            obs["rename_attempted"] = True
            status, _ = rest_patch(token, f"/channels/{args.creator}", {"name": temp_name})
            if status == 403:
                obs["rename_forbidden"] = True
            elif status == 429:
                obs["rename_rate_limited"] = True
                obs["rename_note"] = ("rename PATCH answered 429 (per-channel rename limit); "
                                      "rename skipped")
            elif status is not None and 200 <= status < 300:
                obs["rename_applied"] = True
                obs["rename_name"] = temp_name
                try:
                    seen = rest(token, f"/channels/{args.creator}")
                except Exception:
                    seen = {}
                if seen.get("name") == temp_name:
                    obs["creator"] = creator = seen
                else:
                    obs["rename_note"] = (f"rename PATCH answered {status} but the name "
                                          f"reads back {seen.get('name')!r}")
            else:
                obs["rename_note"] = f"rename PATCH answered {status}; rename skipped"
        obs["channels_before"] = rest(token, f"/guilds/{args.guild}/channels")
        pre_existing_ids = {c.get("id") for c in obs["channels_before"]
                            if isinstance(c.get("id"), str)}
        s.voice(args.guild, args.creator)
        joined = True
        mark = len(s.events)
        parent = creator.get("parent_id")

        def created_room(after, expected_id):
            # The room the bot created for us: the same-window CHANNEL_CREATE
            # that corroborates our own move destination (same id, configured
            # guild, voice type, creator's category). A first-arriving
            # unrelated CREATE never authorizes a probe.
            return next((e["d"] for e in s.events[after:]
                         if e["t"] == "CHANNEL_CREATE"
                         and e["d"].get("id") == expected_id
                         and e["d"].get("guild_id") == args.guild
                         and e["d"].get("type") in VOICE_TYPES
                         and e["d"].get("parent_id") == parent), None)

        def landed(exclude):
            return lambda e: (e["t"] == "VOICE_STATE_UPDATE"
                              and e["d"].get("guild_id") == args.guild
                              and e["d"].get("user_id") == s.me
                              and e["d"].get("channel_id") not in (None, args.creator, *exclude))

        moved = s.pump(landed(()), time.monotonic() + args.create_within)
        obs["moved_to"] = moved["d"]["channel_id"] if moved else None
        # Let the status line, any follow-up writes and a late CREATE land
        # before correlating: the create can arrive before or after our move.
        s.pump(lambda e: False, time.monotonic() + args.settle)
        moved_to = obs["moved_to"]
        if moved_to is None or moved_to == args.creator or moved_to in pre_existing_ids:
            obs["room_created"] = None
            room_id = None
        else:
            obs["room_created"] = created_room(mark, moved_to)
            room_id = moved_to if obs["room_created"] is not None else None
        obs["room_id"] = room_id
        rejoin_mark = None
        if room_id:
            obs["channels"] = rest(token, f"/guilds/{args.guild}/channels")
            obs["statuses"] = [e["d"].get("status") for e in s.events
                               if e["t"] == "VOICE_CHANNEL_STATUS_UPDATE" and e["d"].get("id") == room_id]
            # The limit owner-command round-trip exercises the user_limit
            # field /limit and /unlimit manage, through the same REST PATCH
            # path the bot writes. The privacy check is read-only; the
            # name-panel check reads the rendered room name. Discord creates
            # interactions only for real users, so a bot token cannot invoke
            # the slash commands themselves. Each limit write is verified by
            # a read-back and restored. A failed step records a note (its
            # check FAILs). A limit 403 FAILs (the owner Manage Channels grant
            # was lost after creation); a limit 429 is allowed to SKIP.
            obs["limit_forbidden"] = obs["limit_rate_limited"] = False
            obs["limit_set_ok"] = obs["limit_restored"] = False
            # Set before the probe PATCH: a request Discord applies but whose
            # response is lost (timeout, failed read-back, mismatch) must still
            # be restored, like the creator rename above.
            obs["limit_attempted"] = False
            try:
                seen = rest(token, f"/channels/{room_id}")
                initial = int(seen.get("user_limit") or 0)
                probed = 3 if initial == 2 else 2
                obs["limit_initial"], obs["limit_probed"] = initial, probed

                def restore_uncertain_limit():
                    # Bounded, exception-safe cleanup for an attempted write
                    # without a verified read-back: at most one restore PATCH
                    # to the original value plus one verification read. The
                    # limit check still FAILs with the fixed reason, so an
                    # uncertain write never reads as green.
                    verified = False
                    try:
                        back, _ = rest_patch(token, f"/channels/{room_id}",
                                             {"user_limit": initial})
                        if back is not None and 200 <= back < 300:
                            try:
                                final = int(rest(token, f"/channels/{room_id}")
                                            .get("user_limit") or 0)
                            except Exception:
                                verified = False
                            else:
                                verified = final == initial
                    except Exception:
                        verified = False
                    if verified:
                        obs["limit_note"] = (
                            f"{LIMIT_UNCERTAIN} (room {room_id} restored "
                            f"to {initial}); re-run the synthetic to confirm")
                    else:
                        obs["limit_note"] = (
                            f"{LIMIT_UNCERTAIN} (room {room_id} may still be "
                            f"{probed}, original {initial}); restore "
                            f"user_limit to {initial} manually")

                obs["limit_attempted"] = True
                try:
                    status, _ = rest_patch(token, f"/channels/{room_id}",
                                           {"user_limit": probed})
                except Exception:
                    restore_uncertain_limit()
                else:
                    if status == 403:
                        obs["limit_forbidden"] = True
                    elif status == 429:
                        obs["limit_rate_limited"] = True
                        obs["limit_note"] = ("room user_limit PATCH answered 429 "
                                             "(rate limit); limit skipped")
                    elif status is not None and 200 <= status < 300:
                        try:
                            current = int(rest(token, f"/channels/{room_id}")
                                          .get("user_limit") or 0)
                        except Exception:
                            restore_uncertain_limit()
                        else:
                            if current != probed:
                                restore_uncertain_limit()
                            else:
                                try:
                                    back, _ = rest_patch(
                                        token, f"/channels/{room_id}",
                                        {"user_limit": initial})
                                except Exception:
                                    obs["limit_set_ok"] = True
                                else:
                                    if back is not None and 200 <= back < 300:
                                        try:
                                            final = int(rest(
                                                token, f"/channels/{room_id}")
                                                .get("user_limit") or 0)
                                        except Exception:
                                            obs["limit_set_ok"] = True
                                        else:
                                            if final == initial:
                                                obs["limit_set_ok"] = True
                                                obs["limit_restored"] = True
                                            else:
                                                obs["limit_set_ok"] = True
                                    else:
                                        obs["limit_set_ok"] = True
                    else:
                        restore_uncertain_limit()
            except Exception as exc:
                if "limit_note" not in obs:
                    obs["limit_note"] = f"{type(exc).__name__} during the limit round-trip"
            try:
                room = rest(token, f"/channels/{room_id}")
                roles = rest(token, f"/guilds/{args.guild}/roles")
                product_member = rest(
                    token, f"/guilds/{args.guild}/members/{STAGING_PRODUCT_BOT_ID}")
                guild = rest(token, f"/guilds/{args.guild}")
                obs.update(privacy_facts(room, roles, product_member, args.guild,
                                         STAGING_PRODUCT_BOT_ID, guild.get("owner_id")))
            except Exception as exc:
                obs["privacy_note"] = (
                    f"{type(exc).__name__} reading room privacy facts; could not verify "
                    f"the product bot's staging role ({STAGING_PRODUCT_BOT_ID}) "
                    "effective Manage Roles")
            # Name-panel render: read the room's name after the rename and
            # limit round-trips. No write: the /name panel's Current-name line
            # shows whatever this GET returns.
            try:
                obs["panel_name"] = rest(token, f"/channels/{room_id}").get("name")
            except Exception as exc:
                obs["panel_note"] = f"{type(exc).__name__} reading the room name for the panel"
            # Hop from our own room straight back into the creator: a member
            # doing this must get a new room, not stay in the lobby. The hop
            # counts only when the landing channel is corroborated by a
            # same-window CHANNEL_CREATE for that id (neither the creator,
            # the first room, nor a pre-existing channel); a foreign-guild
            # update, a move into a pre-existing room or an unrelated channel
            # without a CREATE never counts.
            rejoin_mark = len(s.events)
            s.voice(args.guild, args.creator)
            again = s.pump(landed((room_id,)), time.monotonic() + args.create_within)
            rejoin_to = again["d"]["channel_id"] if again else None
            if (rejoin_to is None or rejoin_to == args.creator
                    or rejoin_to == room_id or rejoin_to in pre_existing_ids):
                obs["rejoin_room_id"] = None
            else:
                # Provisional: the delete wait below collects a late CREATE,
                # and the verdict is finalized after it.
                obs["rejoin_room_id"] = rejoin_to
        rooms = [r for r in (room_id, obs.get("rejoin_room_id")) if r]
        s.voice(args.guild, None)
        joined = False
        left = time.monotonic()
        if rooms:
            gone = set()
            s.pump(lambda e: e["t"] == "CHANNEL_DELETE" and e["d"]["id"] in rooms
                   and (gone.add(e["d"]["id"]) or gone >= set(rooms)),
                   time.monotonic() + args.grace + args.delete_slack)
            gone |= {e["d"]["id"] for e in s.events if e["t"] == "CHANNEL_DELETE"}
            obs["deleted"] = set(rooms) <= gone
            obs["delete_seconds"] = round(time.monotonic() - left, 1)
        if room_id and obs.get("rejoin_room_id") and rejoin_mark is not None:
            if created_room(rejoin_mark, obs["rejoin_room_id"]) is None:
                obs["rejoin_room_id"] = None
    finally:
        if joined:
            try:
                s.voice(args.guild, None)
            except OSError:
                pass
        # Restore whenever a rename was attempted and the live name differs
        # from the original: the PATCH may have applied while its response
        # was lost, which the old applied-only gate left as residue.
        if obs.get("rename_attempted") and not obs.get("rename_forbidden"):
            try:
                seen_now = rest(token, f"/channels/{args.creator}")
                if seen_now.get("name") != obs.get("rename_original", ""):
                    status, _ = rest_patch(token, f"/channels/{args.creator}",
                                           {"name": obs.get("rename_original", "")})
                    seen = rest(token, f"/channels/{args.creator}")
                    obs["rename_restored"] = (isinstance(status, int) and 200 <= status < 300
                                              and seen.get("name") == obs.get("rename_original"))
            except Exception:
                obs["rename_restored"] = False
        s.close()
    after = metrics_value(args.staging_url, os.environ.get("TWO_STAGING_METRICS_TOKEN"))
    obs["metrics_delta"] = None if before is None or after is None else int(after - before)
    received_before, refused_before = snap_before
    received_after, refused_after = metrics_snapshot(
        args.staging_url, os.environ.get("TWO_STAGING_METRICS_TOKEN"))
    if received_before is not None and received_after is not None:
        obs["received_delta"] = received_after - received_before
    if refused_before is not None and refused_after is not None:
        delta = {}
        for reason in set(refused_before) | set(refused_after):
            moved = refused_after.get(reason, 0.0) - refused_before.get(reason, 0.0)
            if moved:
                delta[reason] = moved
        obs["refused_delta"] = delta
    obs.setdefault("creator", {"id": args.creator})
    obs.setdefault("channels", [])
    return obs


def parse_args(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--guild", default=os.environ.get("DISCORD_STAGING_GUILD_ID", STAGING_GUILD_ID))
    p.add_argument("--creator", default=STAGING_CREATOR_ID)
    p.add_argument("--staging-url", default=os.environ.get("STAGING_WORKER_URL"))
    p.add_argument("--create-within", type=timing_seconds, default=20.0,
                   metavar="SECONDS")
    p.add_argument("--settle", type=timing_seconds, default=8.0,
                   metavar="SECONDS")
    # The env default stays a string so argparse validates it through the
    # same type: a bare float() here would crash with a traceback instead of
    # the fixed refusal (and accept NaN/inf).
    p.add_argument("--grace", type=timing_seconds,
                   default=os.environ.get("TWO_STAGING_EMPTY_GRACE_SECONDS", "60"),
                   metavar="SECONDS")
    p.add_argument("--delete-slack", type=timing_seconds, default=20.0,
                   metavar="SECONDS")
    p.add_argument("--evidence", default=None)
    p.add_argument("--dry-run", action="store_true",
                   help="print the mutation plan without contacting Discord")
    return p.parse_args(argv)


def print_dry_run_plan(args):
    """Print the mutation plan without contacting Discord or the network.

    Static text plus the fenced guild/creator: no room ids, tokens or
    timestamps are generated, so the plan is byte-identical across runs.
    Fences and timing validation run exactly as on the live path before
    this prints; only the bot-token gate is skipped (a preview needs no
    secret), and no token is ever echoed.
    """
    print("\n".join((
        "dry-run plan for staging voice synthetic (no mutations performed)",
        f"guild: {args.guild}",
        f"creator: {args.creator}",
        f"1. rename: REST PATCH /channels/{args.creator} appends "
        f"{RENAME_MARKER!r} to the creator name, then PATCHes the original "
        "name back (restore)",
        "2. lifecycle: join the creator over the gateway, assert the room "
        "create/move, leave (no channel writes)",
        "3. user_limit: REST PATCH /channels/<room> sets user_limit to the "
        "probed value with a read-back, then PATCHes the initial value "
        "back (restore)",
    )))


def main(argv=None):
    args = parse_args(argv)
    token = os.environ.get("TWO_VOICE_SYNTHETIC_BOT_TOKEN", "").strip()
    try:
        fence(args.guild, args.creator, args.staging_url)
        if not args.dry_run and not token:
            raise Refusal("TWO_VOICE_SYNTHETIC_BOT_TOKEN is not set (dedicated test bot, see docs/voice-synthetic.md)")
    except Refusal as exc:
        print(f"REFUSED: {exc}", file=sys.stderr)
        return 2
    if args.dry_run:
        print_dry_run_plan(args)
        return 0
    try:
        obs = run_live(args, token)
        checks = evaluate(obs)
    except Exception as exc:  # report, never leak the token
        checks = [check("run", False, f"{type(exc).__name__}: {str(exc).replace(token, '***')}")]
        obs = {}
    for c in checks:
        print(f"{c['verdict']:4} {c['check']:9} {c['detail']}")
    if args.evidence:
        with open(args.evidence, "w") as fh:
            json.dump({"checks": checks, "room_id": obs.get("room_id"), "guild": args.guild,
                       "creator": args.creator}, fh, indent=2)
    return 1 if any(c["verdict"] == "FAIL" for c in checks) else 0


if __name__ == "__main__":
    sys.exit(main())
