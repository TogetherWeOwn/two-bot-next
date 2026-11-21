"""Offline help-embed-smoke fixtures; stdlib only, no network access."""

import copy
import io
import json
import os
import re
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_help_embed_smoke as smoke

ROOT = Path(__file__).resolve().parents[1]
STAGING_GUILD = "1545644954272137297"
LIVE_GUILD = "326474832151838730"
OTHER_GUILD = "9999999999999999999"
TEST_BOT = "1555111222333444555"
STAGING_PRODUCT_APP = "1469137636663758888"
OTHER_APP = "1111111111111111111"
CHANNEL = "1546211377000000001"
TOKEN = "RAW_SECRET_SENTINEL_never_disclose_bot_token"
EXPECTED = {"color": 0xFF9C36, "footer_text": "Together We Own",
            "icon_url": "https://imagedelivery.net/cNVnKlHCCMxI9vqDkwBbUQ/"
                        "two-icon1-dark512-4469ca165f03/public"}


ROW_CONTRACTS = {
    "help": ("DEFAULT", 0xFF9C36, "", True, "help text " * 300),
    "help_topic": ("DEFAULT", 0xFF9C36, "", True, "topic text " * 300),
    "announcement": ("DEFAULT", 0xFF9C36, "Announcements", True, "Example announcement"),
    "echo": ("ECHO", 0x9D6AFF, "Echo", True, "Example Echo"),
    "rsvp_ok": ("OK", 0x5FD39A, "Events", False, "Example RSVP saved"),
    "rsvp_error": ("DANGER", 0xFF7A6B, "Events", False, "Example RSVP failed"),
    "voice_ok": ("OK", 0x5FD39A, "Voice", False, "Example room updated"),
    "voice_warn": ("WARN", 0xF2C14E, "Voice", False, "Example room still updating"),
    "voice_error": ("DANGER", 0xFF7A6B, "Voice", False, "Example room update failed"),
}


def payloads(**overrides):
    expected, embeds = {}, {}
    for key, (_, color, surface, has_icon, description) in ROW_CONTRACTS.items():
        text = EXPECTED["footer_text"] + (f" · {surface}" if surface else "")
        icon = EXPECTED["icon_url"] if has_icon else None
        expected[key] = {"color": color, "footer_text": text, "icon_url": icon}
        footer = {"text": text, **({"icon_url": icon} if has_icon else {})}
        embeds[key] = {"type": "rich", "description": description,
                       "color": color, "footer": footer}
    return {"schema": smoke.SCHEMA, "expected": expected, "embeds": embeds,
            **overrides}


class FakeDiscord:
    """Stateful in-memory Discord: posts land in a store, reads serve it."""

    def __init__(self, app=TEST_BOT, bot=True, bot_guilds=(STAGING_GUILD,),
                 channel_guild=STAGING_GUILD,
                 channel_type=0, mutate_read=None, delete_status=204,
                 me_status=200, post_timeout=False, reconcile_status=200,
                 fill_after_timeout=0):
        self.app = app
        self.bot = bot
        self.bot_guilds = bot_guilds
        self.channel_guild = channel_guild
        self.channel_type = channel_type
        self.mutate_read = mutate_read
        self.delete_status = delete_status
        self.me_status = me_status
        self.post_timeout = post_timeout
        self.reconcile_status = reconcile_status
        self.fill_after_timeout = fill_after_timeout
        self._timed_out = False
        self.calls = []
        self.messages = {}
        self.next_id = 1600000000000000000

    def __call__(self, method, path, payload=None):
        self.calls.append((method, path, payload))
        if path == "/users/@me":
            return self.me_status, json.dumps(
                {"id": self.app, "bot": self.bot}).encode()
        if path == "/users/@me/guilds":
            return 200, json.dumps(
                [{"id": guild} for guild in self.bot_guilds]).encode()
        if method == "GET" and path == f"/channels/{CHANNEL}":
            return 200, json.dumps(
                {"id": CHANNEL, "guild_id": self.channel_guild,
                 "type": self.channel_type}).encode()
        if method == "GET" and path.startswith(f"/channels/{CHANNEL}/messages?"):
            from urllib.parse import parse_qs, urlparse
            query = parse_qs(urlparse(path).query)
            after = query.get("after", [None])[0]
            before = query.get("before", [None])[0]
            # Discord documents before/after/around as mutually exclusive:
            # passing more than one is a validation error, not an
            # intersection filter.
            if sum(cursor is not None for cursor in
                   (after, before, query.get("around", [None])[0])) > 1:
                return 400, b"{}"
            limit = int(query.get("limit", ["50"])[0])
            # The anchor read asks for 1; reconciliation pages at the
            # reconciliation page size.
            if limit != 1 and self.reconcile_status != 200:
                return self.reconcile_status, b"{}"
            listed = [m for m in self.messages.values()
                      if (after is None or int(m["id"]) > int(after))
                      and (before is None or int(m["id"]) < int(before))]
            listed.sort(key=lambda m: int(m["id"]), reverse=True)
            return 200, json.dumps(copy.deepcopy(listed[:limit])).encode()
        if method == "POST" and path == f"/channels/{CHANNEL}/messages":
            self.next_id += 1
            message = {"id": str(self.next_id), "channel_id": CHANNEL,
                       "author": {"id": self.app},
                       "embeds": copy.deepcopy(payload["embeds"])}
            self.messages[message["id"]] = message
            if self.post_timeout and not self._timed_out:
                # Discord committed the message but the response was lost.
                self._timed_out = True
                for _ in range(self.fill_after_timeout):
                    self.next_id += 1
                    filler = {"id": str(self.next_id), "channel_id": CHANNEL,
                              "author": {"id": OTHER_APP}, "embeds": []}
                    self.messages[filler["id"]] = filler
                raise TimeoutError("response timed out after the post committed")
            return 201, json.dumps(message).encode()
        message_id = path.rsplit("/", 1)[-1]
        if method == "GET" and path == f"/channels/{CHANNEL}/messages/{message_id}":
            message = copy.deepcopy(self.messages[message_id])
            if self.mutate_read is not None:
                self.mutate_read(message)
            return 200, json.dumps(message).encode()
        if method == "DELETE" and path == f"/channels/{CHANNEL}/messages/{message_id}":
            if self.delete_status == 204:
                self.messages.pop(message_id, None)
                return 204, b""
            return self.delete_status, b"{}"
        raise AssertionError(f"unexpected offline fixture route: {method} {path}")

    def posts(self):
        return [call for call in self.calls if call[0] == "POST"]


class HelpEmbedSmokeTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch.object(
            smoke.urllib.request.OpenerDirector, "open",
            side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.evidence = str(self.dir / "evidence.json")

    def write_payloads(self, data=None):
        path = self.dir / "payloads.json"
        path.write_text(json.dumps(payloads() if data is None else data))
        return str(path)

    def run_main(self, transport, path=None, guild=STAGING_GUILD,
                 channel=CHANNEL, token=TOKEN):
        env = {} if token is None else {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": token}
        argv = ["--payloads", path or self.write_payloads(),
                "--evidence", self.evidence]
        if guild is not None:
            argv += ["--guild-id", guild]
        if channel is not None:
            argv += ["--channel-id", channel]
        with mock.patch.dict(os.environ, env, clear=True):
            out = io.StringIO()
            with redirect_stdout(out), redirect_stderr(out):
                code = smoke.main(argv, transport=transport)
        return code, out.getvalue()

    def evidence_json(self):
        return json.loads(Path(self.evidence).read_text())

    def test_fixture_expectations_match_the_rust_brand_constants(self):
        # The live payload file comes from brand.rs; pin this fixture to it
        # so a brand change that skips the fixture is visible offline.
        source = (ROOT / "crates/discord/src/brand.rs").read_text()
        tokens = (ROOT / "crates/discord/src/brand_tokens.rs").read_text()
        for key, (kind, color, _, _, _) in ROW_CONTRACTS.items():
            with self.subTest(row=key):
                token = re.search(
                    rf"pub const BRAND_DISCORD_EMBED_{kind}: u32 = (0x[0-9A-F]+);", tokens
                )
                self.assertIsNotNone(token, f"the generated kit must define {kind}")
                self.assertEqual(int(token.group(1), 16), color)
        self.assertIn("Self::Default => BRAND_DISCORD_EMBED_DEFAULT", source)
        self.assertIn(f'BRAND_FOOTER_TEXT: &str = "{EXPECTED["footer_text"]}"', source)
        self.assertRegex(
            source, rf'BRAND_ICON_URL: &str =\s*"{re.escape(EXPECTED["icon_url"])}";')

    def test_every_embed_posts_reads_back_and_deletes(self):
        fake = FakeDiscord()
        code, out = self.run_main(fake)
        self.assertEqual(0, code)
        self.assertIn("VERDICT PASS", out)
        posts = fake.posts()
        want = payloads()["embeds"]
        self.assertEqual([[embed] for embed in want.values()],
                         [post[2]["embeds"] for post in posts])
        self.assertTrue(all(post[2]["allowed_mentions"] == {"parse": []} for post in posts))
        self.assertEqual({}, fake.messages, "all messages are deleted")
        methods = [call[0] for call in fake.calls]
        self.assertEqual(["GET", "GET", "GET"] + ["GET", "POST", "GET", "DELETE"] * len(want),
                         methods)
        evidence = self.evidence_json()
        self.assertEqual("PASS", evidence["verdict"])
        self.assertEqual(list(want), [row["name"] for row in evidence["rows"]])
        self.assertTrue(all("message_id" in row for row in evidence["rows"]))
        self.assertNotIn(TOKEN, out)
        self.assertNotIn(TOKEN, Path(self.evidence).read_text())

    def test_assertions_run_on_the_read_back_not_the_post_echo(self):
        def strip_brand(message):
            message["embeds"][0]["color"] = 0

        fake = FakeDiscord(mutate_read=strip_brand)
        code, out = self.run_main(fake)
        self.assertEqual(1, code)
        self.assertIn("read-back colour is 0", out)
        self.assertEqual({}, fake.messages, "failed rows still clean up")

    def test_echo_read_back_rejects_the_correct_colour_for_the_wrong_surface(self):
        def change_echo(message):
            embed = message["embeds"][0]
            if embed["footer"]["text"].endswith(" · Echo"):
                embed["color"] = EXPECTED["color"]

        fake = FakeDiscord(mutate_read=change_echo)
        code, out = self.run_main(fake)
        self.assertEqual(1, code)
        self.assertIn("FAIL    echo: read-back colour", out)
        self.assertIn("PASS    announcement:", out)
        self.assertIn("PASS    rsvp_ok:", out)
        self.assertEqual({}, fake.messages)

    def test_text_only_footers_reject_icons_on_read_back(self):
        def add_icon(message):
            footer = message["embeds"][0]["footer"]
            footer.setdefault("icon_url", EXPECTED["icon_url"])

        fake = FakeDiscord(mutate_read=add_icon)
        code, out = self.run_main(fake)
        self.assertEqual(1, code)
        for key in ("rsvp_ok", "rsvp_error", "voice_ok", "voice_warn", "voice_error"):
            self.assertIn(f"FAIL    {key}: read-back footer icon url does not match", out)
        self.assertEqual({}, fake.messages)

    def test_each_brand_field_mismatch_fails(self):
        cases = {
            "footer text": lambda m: m["embeds"][0]["footer"].update(text="Other"),
            "footer icon": lambda m: m["embeds"][0]["footer"].update(
                icon_url="https://example.com/x.png"),
            "footer missing": lambda m: m["embeds"][0].pop("footer"),
            "no embeds": lambda m: m.update(embeds=[]),
            "two embeds": lambda m: m["embeds"].append(m["embeds"][0]),
            "wrong author": lambda m: m["author"].update(id=OTHER_APP),
        }
        for name, mutate in cases.items():
            with self.subTest(name=name):
                code, out = self.run_main(FakeDiscord(mutate_read=mutate))
                self.assertEqual(1, code)
                self.assertIn("VERDICT FAIL", out)

    def test_one_rows_failure_still_runs_the_other(self):
        seen = []

        def fail_first(message):
            seen.append(message["id"])
            if len(seen) == 1:
                message["embeds"][0]["color"] = 1

        fake = FakeDiscord(mutate_read=fail_first)
        code, out = self.run_main(fake)
        self.assertEqual(1, code)
        self.assertIn("FAIL    help:", out)
        self.assertIn("PASS    help_topic:", out)

    def test_omitted_guild_defaults_to_the_only_admissible_guild(self):
        fake = FakeDiscord()
        code, out = self.run_main(fake, guild=None)
        self.assertEqual(0, code)
        self.assertIn("VERDICT PASS", out)

    def test_cleanup_failure_fails_the_row_with_the_no_retry_exit_code(self):
        fake = FakeDiscord(delete_status=500)
        code, out = self.run_main(fake)
        # Exit 3 is the workflow's do-not-retry signal: a later clean
        # attempt must never read green over messages left behind here.
        self.assertEqual(3, code)
        self.assertIn("cleanup delete failed", out)
        self.assertEqual(len(ROW_CONTRACTS), len(fake.messages))
        evidence = self.evidence_json()
        self.assertTrue(all(row.get("cleanup_failed") for row in evidence["rows"]))

    def test_timed_out_post_is_reconciled_and_removed(self):
        # Discord committed the first post but its response timed out, so
        # the row never learned the message id. Reconciliation against the
        # pre-post anchor must find and delete it; the row still fails
        # (exit 1, retryable) but nothing is left behind.
        fake = FakeDiscord(post_timeout=True)
        code, out = self.run_main(fake)
        self.assertEqual(1, code)
        self.assertIn("reconciled uncertain post", out)
        self.assertEqual({}, fake.messages, "the timed-out post is removed")

    def test_timed_out_post_behind_a_full_page_is_reconciled(self):
        # Ten newer messages from another author fill the first
        # reconciliation page (newest first), so the committed test-bot
        # post sits outside it. Reconciliation must page past them, find
        # and remove the post, and leave the other author's messages.
        fake = FakeDiscord(post_timeout=True, fill_after_timeout=10)
        code, out = self.run_main(fake)
        self.assertEqual(1, code)
        self.assertIn("reconciled uncertain post", out)
        remaining = list(fake.messages.values())
        self.assertEqual(10, len(remaining))
        self.assertTrue(
            all(m["author"]["id"] == OTHER_APP for m in remaining),
            "the timed-out test-bot post is removed; fillers stay")

    def test_reconciliation_keeps_test_bot_messages_at_or_below_the_anchor(self):
        # A test-bot message that predates this attempt (id at or below
        # the pre-post anchor) is not this attempt's to remove, even
        # though backwards paging reads past the anchor to prove the
        # newer range was fully inspected.
        fake = FakeDiscord(post_timeout=True, fill_after_timeout=10)
        old = {"id": str(fake.next_id - 1), "channel_id": CHANNEL,
               "author": {"id": TEST_BOT}, "embeds": []}
        fake.messages[old["id"]] = old
        code, out = self.run_main(fake)
        self.assertEqual(1, code)
        self.assertIn("reconciled uncertain post", out)
        remaining = list(fake.messages.values())
        self.assertIn(old, remaining,
                      "the pre-existing test-bot message stays")
        self.assertEqual(11, len(remaining),
                         "only the timed-out post is removed")

    def test_reconciliation_past_the_page_bound_uses_no_retry_exit(self):
        # If every allowed page comes back full, reconciliation cannot
        # prove it inspected the whole range newer than the anchor, so
        # the row is the exit-3 cleanup class, never a retryable 1.
        fake = FakeDiscord(post_timeout=True, fill_after_timeout=10)
        with mock.patch.object(smoke, "RECONCILE_MAX_PAGES", 1):
            code, out = self.run_main(fake)
        self.assertEqual(3, code)
        self.assertIn("could not be reconciled", out)

    def test_unreconciled_timed_out_post_uses_the_no_retry_exit_code(self):
        # If reconciliation itself cannot run, a committed-but-unseen post
        # may remain: that is the exit-3 cleanup class, never a retryable 1.
        fake = FakeDiscord(post_timeout=True, reconcile_status=500)
        code, out = self.run_main(fake)
        self.assertEqual(3, code)
        self.assertIn("could not be reconciled", out)
        self.assertEqual(1, len(fake.messages))

    def test_fences_refuse_before_any_request(self):
        cases = {
            "live guild": {"guild": LIVE_GUILD},
            "other guild": {"guild": OTHER_GUILD},
            "missing token": {"token": None},
            "empty token": {"token": ""},
            "token with trailing newline": {"token": TOKEN + "\n"},
            "token with trailing space": {"token": TOKEN + " "},
            "token with non-ascii": {"token": TOKEN + "é"},
            "missing channel": {"channel": None},
            "non-snowflake channel": {"channel": "../1"},
            "zero channel": {"channel": "0"},
        }
        for name, kwargs in cases.items():
            with self.subTest(name=name):
                fake = FakeDiscord()
                code, out = self.run_main(fake, **kwargs)
                self.assertEqual(2, code)
                self.assertIn("REFUSED refusing:", out)
                self.assertEqual([], fake.calls)

    def test_payload_fences_refuse_before_any_request(self):
        unbranded = payloads()
        unbranded["embeds"]["help"] = {"type": "rich", "description": "x"}
        missing = payloads()
        missing["embeds"].pop("help_topic")
        missing_new = payloads()
        for key in ("expected", "embeds"):
            missing_new[key].pop("echo")
        wrong_color = payloads()
        wrong_color["embeds"]["echo"]["color"] = EXPECTED["color"]
        non_https = payloads()
        non_https["expected"]["help"]["icon_url"] = "http://example.com/x.png"
        icon_on_ephemeral = payloads()
        icon_on_ephemeral["expected"]["rsvp_ok"]["icon_url"] = EXPECTED["icon_url"]
        icon_on_ephemeral["embeds"]["rsvp_ok"]["footer"]["icon_url"] = EXPECTED["icon_url"]
        cases = {
            "wrong schema": payloads(schema="help_embed_smoke.v0"),
            "unbranded payload embed": unbranded,
            "missing topic embed": missing,
            "missing new row and expectation": missing_new,
            "wrong Echo colour": wrong_color,
            "non-https icon": non_https,
            "icon on a text-only footer": icon_on_ephemeral,
        }
        for name, data in cases.items():
            with self.subTest(name=name):
                fake = FakeDiscord()
                code, out = self.run_main(fake, path=self.write_payloads(data))
                self.assertEqual(2, code)
                self.assertIn("REFUSED refusing:", out)
                self.assertEqual([], fake.calls)

    def test_missing_payload_file_refuses(self):
        fake = FakeDiscord()
        code, out = self.run_main(fake, path=str(self.dir / "absent.json"))
        self.assertEqual(2, code)
        self.assertIn("payload file unreadable", out)
        self.assertEqual([], fake.calls)

    def test_wrong_application_identity_posts_nothing(self):
        cases = {
            "staging product bot token": (
                {"app": STAGING_PRODUCT_APP},
                "token belongs to a product application"),
            "live product bot token": (
                {"app": "1539711683898118154"},
                "token belongs to a product application"),
            "former staging bot token": (
                {"app": "1537629682449649724"},
                "token belongs to a product application"),
            "user account token": (
                {"bot": False},
                "token does not belong to a bot account"),
            "bot also in another guild": (
                {"bot_guilds": (STAGING_GUILD, OTHER_GUILD)},
                "test bot is not a member of exactly the staging guild"),
            "bot in no guild": (
                {"bot_guilds": ()},
                "test bot is not a member of exactly the staging guild"),
            "bot only in the live guild": (
                {"bot_guilds": (LIVE_GUILD,)},
                "test bot is not a member of exactly the staging guild"),
        }
        for name, (kwargs, message) in cases.items():
            with self.subTest(name=name):
                fake = FakeDiscord(**kwargs)
                code, out = self.run_main(fake)
                self.assertEqual(1, code)
                self.assertIn(message, out)
                self.assertEqual([], fake.posts())

    def test_channel_outside_staging_guild_posts_nothing(self):
        for kwargs in ({"channel_guild": OTHER_GUILD}, {"channel_type": 2}):
            with self.subTest(**kwargs):
                fake = FakeDiscord(**kwargs)
                code, _ = self.run_main(fake)
                self.assertEqual(1, code)
                self.assertEqual([], fake.posts())

    def test_unauthorized_is_a_fixed_message_without_the_token(self):
        fake = FakeDiscord(me_status=401)
        code, out = self.run_main(fake)
        self.assertEqual(1, code)
        self.assertIn("credential refused (401)", out)
        self.assertNotIn(TOKEN, out)
        self.assertEqual([], fake.posts())

    def test_malformed_token_refuses_without_reaching_the_http_layer(self):
        # The stdlib header validation error names the header value, so a
        # malformed token must refuse in the fence, before any request is
        # built, and the sentinel must appear on neither stream nor disk.
        fake = FakeDiscord()
        code, out = self.run_main(fake, token=TOKEN + "\r\n")
        self.assertEqual(2, code)
        self.assertIn("test bot token is malformed", out)
        self.assertNotIn(TOKEN, out)
        self.assertEqual([], fake.calls)
        self.assertNotIn(TOKEN, Path(self.evidence).read_text())


if __name__ == "__main__":
    unittest.main()
