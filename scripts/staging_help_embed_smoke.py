#!/usr/bin/env python3
"""Live staging /help embed smoke: post, read back, assert the brand (stdlib only).

Discord creates interactions only for real users, so a bot token cannot
invoke /help and capture its reply. This smoke proves the rendered half
that unit tests cannot: the exact embeds the two help builders produce
(help_response for /help, help_detail_response for /help <topic>, emitted
after the executor sanitizer by the `help_embed_smoke_payloads` example in
crates/discord) are posted to one staging test channel as ordinary
messages, read back with GET channels/{id}/messages/{id}, and asserted to
carry the brand colour, footer text and footer icon URL that the payload
file names. Both messages are deleted afterwards, pass or fail. It proves
a rendered Discord message serves the builder payload with the brand
intact; it is not a slash-command invocation proof (no human-free one
exists) and claims nothing about the gateway path.

The posting identity is the dedicated staging test bot (TWO Voice
Synthetic (staging), the same bot the voice synthetic drives), never the
staging product bot: the product bot's token stays out of GitHub by
recorded decision (docs/staging-slash-smoke.md "Why it is not a GitHub
workflow"), while the test bot's token already lives in the
`voice-synthetic` GitHub environment and the bot lives only in the
staging guild.

Fences, all before the first request:
  * the guild id must be exactly the TWO Staging guild (the shared fence
    in staging_automation_read_smoke; the live guild refuses by name);
  * the test bot token comes only from $TWO_VOICE_SYNTHETIC_BOT_TOKEN and
    is never printed, logged or written to evidence; a 401 refuses with a
    fixed message and no substitute credential is ever tried;
  * the payload file must carry schema help_embed_smoke.v1, both embeds
    and the expected colour/footer/icon, and each payload embed must
    already match those expectations;
  * the channel id must be a canonical snowflake. Its staging-guild and
    text-type membership is then verified with GET channels/{id} before
    anything is posted, and the token must belong to a bot that is none
    of the product applications and is a member of exactly the staging
    guild (GET users/@me, GET users/@me/guilds).

Exit status: 0 PASS, 1 a check failed, 2 refused by a fence (nothing was
posted), 3 a cleanup delete failed — or a post whose outcome was lost
(timeout, 5xx, unreadable answer) could not be reconciled against the
pre-post channel anchor — and a posted message may remain in the test
channel. A lost post that reconciliation removes (or proves absent)
stays a retryable exit 1. Status 3 is distinct so the workflow fails at once instead
of retrying: a later clean attempt must never read green over a message
an earlier attempt left behind. Evidence carries row results, message ids
and asserted values only — never the token, a request body or a remote
error body.

Usage:
  TWO_VOICE_SYNTHETIC_BOT_TOKEN=... DISCORD_STAGING_GUILD_ID=... \\
  DISCORD_STAGING_HELP_SMOKE_CHANNEL_ID=... \\
      python3 scripts/staging_help_embed_smoke.py --payloads FILE [--evidence FILE]
"""

import argparse
from dataclasses import dataclass, field
import http.client
import json
import os
from pathlib import Path
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_automation_read_smoke as fenced

API = fenced.API
SCHEMA = "help_embed_smoke.v1"
USER_AGENT = "two-bot-next-staging-help-embed-smoke/1.0"
BODY_CAP = fenced.BODY_CAP
PAYLOAD_CAP = 1 << 20
TIMEOUT_SECONDS = 15
# Guild text (0) and announcement (5) channels accept bot message posts.
TEXT_CHANNEL_TYPES = (0, 5)
EMBED_KEYS = ("help", "help_topic")
# The product applications a test-bot token must never turn out to be:
# staging product bot (the shared fence value), live bot and the superseded
# staging bot (public ids from two_bot_core::backup::guild_config).
PRODUCT_APPLICATION_IDS = (
    fenced.STAGING_APPLICATION_ID,
    "1539711683898118154",
    "1537629682449649724",
)
# Reconciliation pages newest-first through the range newer than the
# pre-post anchor. Reaching the anchor — or a short page at the start of
# the channel — proves the whole range was inspected; a full page proves
# nothing, so traversal continues with a `before` cursor and gives up
# (no-retry exit 3) past this many pages.
RECONCILE_PAGE_SIZE = 10
RECONCILE_MAX_PAGES = 10


class Refused(Exception):
    """A fence refused the run before anything was posted (exit 2)."""


class SmokeFail(Exception):
    """One check's fixed failure reason (exit 1 for that row)."""


class UncertainOutcome(SmokeFail):
    """A request whose server-side effect is unknown (transport loss, a
    5xx or an unreadable answer to a POST). A post that fails this way may
    have committed, so the caller must reconcile before any retry."""


@dataclass
class Row:
    name: str
    result: str  # pass | fail
    actual: str
    message_id: str | None = None
    duration_ms: int = 0
    cleanup_failed: bool = False

    def line(self):
        return f"{self.result.upper():7} {self.name}: {self.actual}"

    def record(self):
        row = {"name": self.name, "result": self.result,
               "actual": self.actual, "duration_ms": self.duration_ms}
        if self.message_id is not None:
            row["message_id"] = self.message_id
        if self.cleanup_failed:
            row["cleanup_failed"] = True
        return row


@dataclass
class Report:
    rows: list = field(default_factory=list)
    refused: str | None = None

    @property
    def verdict(self):
        if self.refused is not None:
            return "REFUSED"
        return "PASS" if all(row.result == "pass" for row in self.rows) else "FAIL"

    def exit_code(self):
        if any(row.cleanup_failed for row in self.rows):
            return 3
        return {"PASS": 0, "FAIL": 1, "REFUSED": 2}[self.verdict]

    def evidence(self):
        out = {"schema": SCHEMA, "verdict": self.verdict,
               "rows": [row.record() for row in self.rows]}
        if self.refused is not None:
            out["refusal"] = self.refused
        return out


def load_payloads(path):
    """Parse and fence the Rust-emitted payload file; fixed refusals only."""
    try:
        raw = Path(path).read_bytes()
    except OSError:
        raise Refused("refusing: payload file unreadable")
    if len(raw) > PAYLOAD_CAP:
        raise Refused("refusing: payload file over the size cap")
    try:
        payloads = fenced._loads_bounded(
            raw, object_pairs_hook=fenced._reject_duplicate_keys)
    except (ValueError, UnicodeDecodeError, RecursionError):
        raise Refused("refusing: payload file is not valid bounded JSON")
    if not isinstance(payloads, dict) or payloads.get("schema") != SCHEMA:
        raise Refused("refusing: payload schema is not " + SCHEMA)
    expected = payloads.get("expected")
    embeds = payloads.get("embeds")
    if not isinstance(expected, dict) or not isinstance(embeds, dict):
        raise Refused("refusing: payload carries no expectations or embeds")
    color = expected.get("color")
    footer_text = expected.get("footer_text")
    icon_url = expected.get("icon_url")
    if type(color) is not int or not (0 <= color <= 0xFFFFFF):
        raise Refused("refusing: payload expected colour is not a Discord colour")
    if not isinstance(footer_text, str) or not footer_text:
        raise Refused("refusing: payload expected footer text is missing")
    if not isinstance(icon_url, str) or not icon_url.startswith("https://"):
        raise Refused("refusing: payload expected icon url is not https")
    for key in EMBED_KEYS:
        embed = embeds.get(key)
        if not isinstance(embed, dict):
            raise Refused(f"refusing: payload embed {key} is missing")
        if brand_fault(embed, expected) is not None:
            raise Refused(f"refusing: payload embed {key} already misses the brand")
        description = embed.get("description")
        if not isinstance(description, str) or not description:
            raise Refused(f"refusing: payload embed {key} has no description")
    return expected, {key: embeds[key] for key in EMBED_KEYS}


def brand_fault(embed, expected):
    """Fixed phrase for the first brand mismatch on one embed, or None."""
    if not isinstance(embed, dict):
        return "embed is not an object"
    if embed.get("color") != expected["color"]:
        return f"colour is {embed.get('color')!r}, expected {expected['color']}"
    footer = embed.get("footer")
    if not isinstance(footer, dict):
        return "footer is missing"
    if footer.get("text") != expected["footer_text"]:
        return "footer text does not match"
    if footer.get("icon_url") != expected["icon_url"]:
        return "footer icon url does not match"
    return None


class _RefuseRedirect(urllib.request.HTTPRedirectHandler):
    """Never replay the bot token to a redirect target; body stays unread."""

    def http_error_301(self, req, fp, code, msg, headers):
        fp.close()
        raise SmokeFail("authenticated request answered a redirect (not followed)")

    http_error_302 = http_error_303 = http_error_307 = http_error_308 = http_error_301


def make_transport(token):
    """Build the only network function; the token never leaves this closure.

    transport(method, path, payload=None) -> (status, body bytes).
    """

    def transport(method, path, payload=None):
        data = None
        headers = {"Authorization": "Bot " + token, "User-Agent": USER_AGENT,
                   "Cache-Control": "no-cache"}
        if payload is not None:
            data = json.dumps(payload).encode()
            headers["Content-Type"] = "application/json"
        try:
            request = urllib.request.Request(
                API + path, data=data, method=method, headers=headers)
            with urllib.request.build_opener(_RefuseRedirect).open(
                    request, timeout=TIMEOUT_SECONDS) as response:
                return response.status, response.read(BODY_CAP + 1)
        except urllib.error.HTTPError as error:
            return error.code, error.read(BODY_CAP + 1)
        except ValueError:
            # Header validation/encoding failures name the header value in
            # the exception text; never let that reach a traceback or log.
            raise SmokeFail("credential is malformed; request not sent")

    return transport


def call_json(transport, method, path, payload=None, expect=(200,)):
    """One authed call returning parsed JSON; fixed-vocabulary failures."""
    try:
        status, body = transport(method, path, payload)
    except (OSError, http.client.HTTPException) as error:
        raise UncertainOutcome(
            f"discord did not respond ({error.__class__.__name__})")
    if status == 401:
        raise SmokeFail("credential refused (401); rotation is an operator decision")
    if status == 403:
        raise SmokeFail("test bot lacks access (403)")
    if status == 429:
        raise SmokeFail("rate limited (429); rerun later")
    if status not in expect:
        # A 5xx to a POST may still have committed the message.
        fail = UncertainOutcome if method == "POST" and status >= 500 else SmokeFail
        raise fail(f"discord answered {status}, expected {'/'.join(map(str, expect))}")
    if not body:
        return None
    # An unreadable POST answer hides whether the message committed.
    fail = UncertainOutcome if method == "POST" else SmokeFail
    if len(body) > BODY_CAP:
        raise fail("discord answered over the body cap")
    try:
        return fenced._loads_bounded(
            body, object_pairs_hook=fenced._reject_duplicate_keys)
    except (ValueError, UnicodeDecodeError, RecursionError):
        raise fail("discord answered with invalid JSON")


def check_identity(transport, guild_id):
    """The token must be a dedicated staging-only test bot.

    The test bot's own id is deliberately not pinned (it is not a repo
    constant); instead the token must belong to a bot account that is
    none of the product applications and whose only guild membership is
    the fenced staging guild. A product-bot token, a user token, or a
    bot that also lives elsewhere all fail before anything is posted.
    """
    me = call_json(transport, "GET", "/users/@me")
    if not isinstance(me, dict) or not fenced.is_snowflake(me.get("id")):
        raise SmokeFail("token identity read answered without a valid bot id")
    if me.get("bot") is not True:
        raise SmokeFail("token does not belong to a bot account")
    if me["id"] in PRODUCT_APPLICATION_IDS:
        raise SmokeFail("token belongs to a product application, not the test bot")
    guilds = call_json(transport, "GET", "/users/@me/guilds")
    if (not isinstance(guilds, list) or len(guilds) != 1
            or not isinstance(guilds[0], dict)
            or guilds[0].get("id") != guild_id):
        raise SmokeFail("test bot is not a member of exactly the staging guild")
    return me["id"]


def check_channel(transport, channel_id, guild_id):
    """The target must be a text channel of the fenced staging guild."""
    channel = call_json(transport, "GET", f"/channels/{channel_id}")
    if not isinstance(channel, dict):
        raise SmokeFail("channel read answered without an object")
    if channel.get("guild_id") != guild_id:
        raise SmokeFail("channel is not in the staging guild")
    kind = channel.get("type")
    if type(kind) is not int or kind not in TEXT_CHANNEL_TYPES:
        raise SmokeFail("channel is not a guild text channel")


def assert_message(message, expected, application_id, channel_id, message_id):
    """Bind one read-back message to the post and assert the brand triple."""
    if not isinstance(message, dict):
        raise SmokeFail("read-back answered without an object")
    if message.get("id") != message_id:
        raise SmokeFail("read-back id disagrees with the posted message")
    if message.get("channel_id") != channel_id:
        raise SmokeFail("read-back is in a different channel")
    author = message.get("author")
    if not isinstance(author, dict) or author.get("id") != application_id:
        raise SmokeFail("read-back author is not the test bot")
    embeds = message.get("embeds")
    if not isinstance(embeds, list) or len(embeds) != 1:
        raise SmokeFail("read-back carries other than exactly one embed")
    fault = brand_fault(embeds[0], expected)
    if fault is not None:
        raise SmokeFail("read-back " + fault)


def latest_message_id(transport, channel_id):
    """Newest message id in the channel (the pre-post anchor), or None."""
    messages = call_json(transport, "GET",
                         f"/channels/{channel_id}/messages?limit=1")
    if not isinstance(messages, list) or len(messages) > 1:
        raise SmokeFail("channel history read answered without a message list")
    if not messages:
        return None
    newest = messages[0]
    if not isinstance(newest, dict) or not fenced.is_snowflake(newest.get("id")):
        raise SmokeFail("channel history read answered without a valid message id")
    return newest["id"]


def reconcile_uncertain_post(transport, channel_id, application_id, anchor_id):
    """Delete every test-bot message newer than the pre-post anchor.

    Runs when a post's outcome is unknown: Discord may have committed it
    without the row ever learning its id. The channel is a dedicated test
    channel and the workflow's concurrency group serializes smokes, so
    any newer test-bot message is this attempt's. History reads return
    newest first, so a full first page can hide the committed post
    behind newer messages from other authors; page backwards with a
    `before` cursor until the anchor (or a short page at the start of
    the channel) proves the whole range newer than the anchor was
    inspected. Discord treats `before`/`after`/`around` as mutually
    exclusive, so the anchor is a stop condition on the walk, never a
    second cursor. Returns how many were removed; raises SmokeFail
    when removal — or complete inspection — cannot be established.
    """
    removed = 0
    before = None
    anchor = int(anchor_id) if anchor_id is not None else None
    for _ in range(RECONCILE_MAX_PAGES):
        path = f"/channels/{channel_id}/messages?limit={RECONCILE_PAGE_SIZE}"
        if before is not None:
            path += f"&before={before}"
        messages = call_json(transport, "GET", path)
        if not isinstance(messages, list):
            raise SmokeFail("reconciliation read answered without a message list")
        page_oldest = None
        reached_anchor = False
        for message in messages:
            if not isinstance(message, dict):
                raise SmokeFail("reconciliation read answered without message objects")
            if not fenced.is_snowflake(message.get("id")):
                raise SmokeFail("reconciliation read answered without a valid message id")
            message_id = int(message["id"])
            if before is not None and message_id >= int(before):
                raise SmokeFail("reconciliation read answered outside the paged range")
            if page_oldest is None or message_id < page_oldest:
                page_oldest = message_id
            if anchor is not None and message_id <= anchor:
                # History reads are newest-first, so the anchored range
                # is fully inspected once the walk reaches the anchor;
                # older messages predate this attempt and stay.
                reached_anchor = True
                continue
            author = message.get("author")
            if not isinstance(author, dict) or author.get("id") != application_id:
                continue
            call_json(transport, "DELETE",
                      f"/channels/{channel_id}/messages/{message['id']}",
                      expect=(204,))
            removed += 1
        if reached_anchor or len(messages) < RECONCILE_PAGE_SIZE:
            return removed
        before = str(page_oldest)
    raise SmokeFail(
        "reconciliation could not inspect every message newer than the "
        "anchor within the page bound")


def smoke_one(transport, row_name, embed, expected, application_id, channel_id):
    """Post one embed, assert on the read-back (never the POST echo), delete."""
    started = time.monotonic()
    message_id = None
    cleanup_failed = False

    def done(result, actual):
        return Row(row_name, result, actual, message_id=message_id,
                   duration_ms=round((time.monotonic() - started) * 1000),
                   cleanup_failed=cleanup_failed)

    payload = {"embeds": [embed], "allowed_mentions": {"parse": []}}
    failure = None

    try:
        anchor_id = latest_message_id(transport, channel_id)
    except SmokeFail as error:
        return done("fail", str(error))
    try:
        posted = call_json(transport, "POST", f"/channels/{channel_id}/messages",
                           payload, expect=(200, 201))
        if not isinstance(posted, dict) or not fenced.is_snowflake(posted.get("id")):
            raise UncertainOutcome("post answered without a valid message id")
        message_id = posted["id"]
        read_back = call_json(
            transport, "GET", f"/channels/{channel_id}/messages/{message_id}")
        assert_message(read_back, expected, application_id, channel_id, message_id)
    except UncertainOutcome as error:
        failure = str(error)
        if message_id is None:
            # The post may have committed unseen. Reconcile against the
            # anchor before this row may fail retryably; if removal cannot
            # be established, this is the no-retry cleanup class (exit 3).
            try:
                removed = reconcile_uncertain_post(
                    transport, channel_id, application_id, anchor_id)
                failure += f"; reconciled uncertain post ({removed} removed)"
            except SmokeFail as reconcile_error:
                cleanup_failed = True
                failure += (f"; uncertain post could not be reconciled "
                            f"({reconcile_error}); a message may remain in "
                            f"the test channel")
    except SmokeFail as error:
        failure = str(error)
    if message_id is not None:
        try:
            call_json(transport, "DELETE",
                      f"/channels/{channel_id}/messages/{message_id}",
                      expect=(204,))
        except SmokeFail as error:
            cleanup_failed = True
            cleanup = f"cleanup delete failed ({error}); message left in the test channel"
            failure = cleanup if failure is None else f"{failure}; {cleanup}"
    if failure is not None:
        return done("fail", failure)
    return done("pass", "read-back colour, footer text and icon url match; message deleted")


def fences(guild_id, token, channel_id):
    """Every no-request refusal; returns (guild, channel) once admitted."""
    try:
        guild = fenced.guild_fence(guild_id)
    except fenced.SmokeError as error:
        raise Refused(str(error))
    if not token:
        raise Refused("refusing: no test bot token given "
                      "($TWO_VOICE_SYNTHETIC_BOT_TOKEN)")
    # A token with whitespace, control or non-ASCII characters (for example
    # a pasted trailing newline) must refuse here: the HTTP layer's header
    # validation exception names the header value, so it must never be
    # reached with a malformed credential.
    if (not isinstance(token, str) or token != token.strip()
            or any(ord(ch) < 33 or ord(ch) > 126 for ch in token)):
        raise Refused("refusing: test bot token is malformed")
    if not channel_id:
        raise Refused("refusing: no staging test channel id given "
                      "($DISCORD_STAGING_HELP_SMOKE_CHANNEL_ID)")
    if not fenced.is_snowflake(channel_id):
        raise Refused("refusing: test channel id is not a canonical snowflake")
    return guild, channel_id


def run(payload_path, guild_id, token, channel_id, transport=None):
    """Fences, identity, channel, then both embed round trips."""
    report = Report()
    try:
        guild, channel = fences(guild_id, token, channel_id)
        expected, embeds = load_payloads(payload_path)
    except Refused as error:
        report.refused = str(error)
        return report
    net = transport if transport is not None else make_transport(token)
    try:
        application_id = check_identity(net, guild)
        check_channel(net, channel, guild)
    except SmokeFail as error:
        report.rows.append(Row("preflight", "fail", str(error)))
        return report
    for key in EMBED_KEYS:
        report.rows.append(
            smoke_one(net, key, embeds[key], expected, application_id, channel))
    return report


def main(argv=None, transport=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--payloads", required=True,
                        help="payload file from the help_embed_smoke_payloads example")
    parser.add_argument(
        "--guild-id",
        default=os.environ.get("DISCORD_STAGING_GUILD_ID", fenced.STAGING_GUILD_ID),
        help="staging guild id (default: $DISCORD_STAGING_GUILD_ID, else the "
             "one admissible staging guild; any other value refuses)")
    parser.add_argument(
        "--channel-id",
        default=os.environ.get("DISCORD_STAGING_HELP_SMOKE_CHANNEL_ID"),
        help="staging test channel id (default: $DISCORD_STAGING_HELP_SMOKE_CHANNEL_ID)")
    parser.add_argument("--evidence", help="write the evidence JSON here")
    args = parser.parse_args(argv)
    token = os.environ.get("TWO_VOICE_SYNTHETIC_BOT_TOKEN", "")
    report = run(args.payloads, args.guild_id, token, args.channel_id,
                 transport=transport)
    if report.refused is not None:
        print(f"REFUSED {report.refused}")
    for row in report.rows:
        print(row.line())
    print(f"VERDICT {report.verdict}")
    if args.evidence:
        Path(args.evidence).write_text(
            json.dumps(report.evidence(), indent=2, sort_keys=True) + "\n")
    return report.exit_code()


if __name__ == "__main__":
    sys.exit(main())
