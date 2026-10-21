#!/usr/bin/env python3
"""Protected acceptance tests: voice-synthetic probes bind to owned rooms.

Offline only. A scripted fake gateway feeds event batches to the real
``run_live``/``main`` (the real ``until`` closures decide every match), and a
stateful fake REST records every PATCH subject while rooms mutate under it. No
network, no tokens (the token below is a fake). Exit codes and the JSON
evidence file are asserted, so observations stay consistent with verdicts.

Contract pinned for the Builder (V1 of the roadmap):

- Owned-room binding: every room probe/write targets exactly the synthetic
  member's observed destination (``moved_to``) in the configured guild, and
  only when a same-window ``CHANNEL_CREATE`` with that same id, voice type
  and the creator's ``parent_id`` corroborates it. A first-arriving unrelated
  CREATE, a foreign-guild CREATE/update, a pre-existing room, or a missing
  move never authorizes a PATCH: ``room_id`` is None, the create check FAILs,
  and no room PATCH is sent.
- Rejoin correlation: ``rejoin_room_id`` counts only when the hop lands in
  the configured guild in a channel corroborated by a same-window
  ``CHANNEL_CREATE`` for that id that is neither the creator, the first room,
  nor a pre-existing channel. Otherwise None plus a FAIL check.
- Uncertain writes: any attempted ``user_limit`` PATCH without a verified
  read-back (lost/ambiguous response, failed read, mismatch, or a non-2xx
  status other than 403/429) enters a bounded cleanup: at most one restore
  PATCH to the original value plus one verification read, exception-safe.
  The limit check still FAILs with a fixed reason, the room id, the original
  value and a concrete next action; exit 1, never green.
- Fixed vocabulary (substrings of the limit detail): ``LIMIT_UNCERTAIN`` in
  every uncertain-write detail; a verified cleanup also names the room, the
  original value and ``re-run``; a failed/unverified cleanup also names the
  room, the original value, ``restore`` and ``manually``.
- Refusals record a None subject and a FAIL check; they never raise out of
  ``run_live``. Preserved behavior: the live-guild fence (exit 2, zero REST
  calls), the creator rename attempt/restore, the 403 FAIL and 429 SKIP limit
  semantics (no restore either way), and token redaction.

Protected file: the Builder builds on this branch head and never edits this
file. Test corrections go back to the author.

Builder notes (not test assertions):

- Wire this file into CI with one voice-gated line mirroring the existing
  ``test_staging_voice_synthetic_gateway_intake_acceptance.py`` invocation in
  ``check.yml``.
- The natural fix sites are the ``created_room``/``landed`` closures, the
  ``room_id`` fallback, the rejoin correlation, and a bounded limit-cleanup
  path that records an attempted write before the probe PATCH.
"""

import contextlib
import copy
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_voice_synthetic as synth

FAKE_TOKEN = "TOK"
GUILD = synth.STAGING_GUILD_ID
CREATOR = synth.STAGING_CREATOR_ID
ME = "900"
OWN = "5001"
FOREIGN = "5002"
PRE = "5003"
REJOIN = "5004"
STRAY = "5005"
FOREIGN2 = "5006"
CAT = "4000"
OTHER_GUILD = "9999"
TEMPLATE_NAME = "\U0001f344 Synthetic’s workshop"
ORIGINAL_NAME = "Create a Lobby"
TEMP_NAME = ORIGINAL_NAME + synth.RENAME_MARKER
RESIDUE_NAME = TEMP_NAME  # a previous run's unrestored temp name: rename skipped

LIMIT_UNCERTAIN = "room user_limit write was not verified"
INITIAL_LIMIT = 0
PROBED_LIMIT = 2


def overwrites_for(member_id):
    return [{"id": GUILD, "type": 0, "allow": synth.CONNECT, "deny": 0},
            {"id": member_id, "type": 1, "allow": synth.OWNER_BITS, "deny": 0}]


def voice_update(channel_id, guild_id=GUILD, user_id=ME):
    return {"t": "VOICE_STATE_UPDATE",
            "d": {"guild_id": guild_id, "channel_id": channel_id,
                  "user_id": user_id}}


def channel_create(channel_id, guild_id=GUILD, name=TEMPLATE_NAME,
                   parent_id=CAT):
    return {"t": "CHANNEL_CREATE",
            "d": {"id": channel_id, "guild_id": guild_id, "type": 2,
                  "parent_id": parent_id, "name": name,
                  "permission_overwrites": overwrites_for(ME)}}


def room_state(channel_id, name=TEMPLATE_NAME, user_limit=INITIAL_LIMIT,
               position=17, owner=ME):
    return {"id": channel_id, "type": 2, "parent_id": CAT, "name": name,
            "position": position, "user_limit": user_limit,
            "permission_overwrites": overwrites_for(owner)}


def creator_state(name):
    return {"id": CREATOR, "type": 2, "parent_id": CAT, "name": name,
            "position": 16}


GUILD_CREATE = {"t": "GUILD_CREATE", "d": {"id": GUILD}}


def status_update(channel_id):
    return {"t": "VOICE_CHANNEL_STATUS_UPDATE",
            "d": {"id": channel_id, "status": "\U0001f4ac chatting · 1 person"}}


def channel_delete(channel_id):
    return {"t": "CHANNEL_DELETE", "d": {"id": channel_id}}


class FakeSession:
    """Scripted Session stand-in: one event batch per pump call.

    The real ``until`` closures built inside ``run_live`` decide every match,
    so whatever correlation the implementation applies is what gets tested.
    When the script runs out, further pumps see no events (return None).
    """

    def __init__(self, batches, me=ME):
        self._batches = [list(batch) for batch in batches]
        self.events = []
        self.me = me
        self.seq = None
        self.voices = []
        self.closed = False

    def pump(self, until, deadline):
        batch = self._batches.pop(0) if self._batches else []
        for raw in batch:
            event = {"t": raw["t"], "d": raw["d"]}
            self.events.append(event)
            if until(event):
                return event
        return None

    def voice(self, guild_id, channel_id):
        self.voices.append((guild_id, channel_id))

    def close(self):
        self.closed = True


class FakeRest:
    """Stateful offline Discord REST: rooms mutate under PATCH, reads see it.

    ``patch_plan`` entries, consumed in PATCH order: ``"ok"``,
    ``("status", code)``, ``("raise", exc)``, ``("apply_then_raise", exc)``.
    ``get_plan`` maps a path to a list consumed in GET order: ``"ok"``,
    ``("raise", exc)``, ``("override", patch_dict)``.
    """

    def __init__(self, *, rooms, guild_lists, creator_name):
        self.rooms = {cid: copy.deepcopy(state)
                      for cid, state in rooms.items()}
        self.rooms[CREATOR] = creator_state(creator_name)
        self._guild_lists = [copy.deepcopy(entry) for entry in guild_lists]
        self._guild_list_calls = 0
        self.roles = [{"id": GUILD, "name": "Staging QA",
                       "permissions": synth.VIEW | synth.MANAGE_ROLES}]
        self.member = {"roles": []}
        self.guild = {"id": GUILD, "owner_id": "1"}
        self.gets = []
        self.patches = []
        self.patch_plan = []
        self.get_plan = {}

    def rest(self, token, path, timeout=20):
        assert token == FAKE_TOKEN, "test bug: unexpected token"
        self.gets.append(path)
        plan = self.get_plan.get(path, [])
        behavior = plan.pop(0) if plan else "ok"
        if isinstance(behavior, tuple) and behavior[0] == "raise":
            raise behavior[1]
        state = self._state_for(path)
        if isinstance(behavior, tuple) and behavior[0] == "override":
            state = copy.deepcopy(state)
            state.update(behavior[1])
            return state
        return copy.deepcopy(state)

    def rest_patch(self, token, path, payload, timeout=20):
        assert token == FAKE_TOKEN, "test bug: unexpected token"
        assert path.startswith("/channels/"), f"test bug: PATCH {path}"
        cid = path.rsplit("/", 1)[1]
        self.patches.append((cid, copy.deepcopy(payload)))
        behavior = self.patch_plan.pop(0) if self.patch_plan else "ok"
        if behavior == "ok":
            self.rooms[cid].update(copy.deepcopy(payload))
            return 200, copy.deepcopy(self.rooms[cid])
        if isinstance(behavior, tuple) and behavior[0] == "status":
            return behavior[1], None
        if isinstance(behavior, tuple) and behavior[0] == "apply_then_raise":
            self.rooms[cid].update(copy.deepcopy(payload))
            raise behavior[1]
        if isinstance(behavior, tuple) and behavior[0] == "raise":
            raise behavior[1]
        raise AssertionError(f"test bug: bad patch behavior {behavior!r}")

    def _state_for(self, path):
        if path.startswith("/channels/"):
            cid = path.rsplit("/", 1)[1]
            assert cid in self.rooms, f"test bug: unscripted GET {path}"
            return self.rooms[cid]
        if path == f"/guilds/{GUILD}/channels":
            idx = min(self._guild_list_calls, len(self._guild_lists) - 1)
            self._guild_list_calls += 1
            return self._guild_lists[idx]
        if path == f"/guilds/{GUILD}/roles":
            return self.roles
        if path == f"/guilds/{GUILD}/members/{synth.STAGING_PRODUCT_BOT_ID}":
            return self.member
        if path == f"/guilds/{GUILD}":
            return self.guild
        raise AssertionError(f"test bug: unscripted GET {path}")

    def gets_to(self, channel_id):
        return sum(1 for path in self.gets
                   if path == f"/channels/{channel_id}")


def run_with_fakes(extra_argv, session, fake):
    """Drive the real ``main`` with a scripted gateway and stateful REST."""
    with tempfile.TemporaryDirectory() as tmp:
        out = str(Path(tmp) / "out.json")
        argv = list(extra_argv) + ["--evidence", out]
        with mock.patch.object(synth, "Session", return_value=session), \
                mock.patch.object(synth, "rest", new=fake.rest), \
                mock.patch.object(synth, "rest_patch", new=fake.rest_patch), \
                mock.patch.object(synth, "metrics_value",
                                  new=lambda *args: None), \
                mock.patch.object(synth, "metrics_snapshot",
                                  new=lambda *args: (None, None)), \
                mock.patch.dict(os.environ,
                                {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": FAKE_TOKEN},
                                clear=False):
            for var in ("TWO_STAGING_METRICS_TOKEN", "STAGING_WORKER_URL",
                        "DISCORD_STAGING_GUILD_ID"):
                os.environ.pop(var, None)
            stdout, stderr = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(stdout), \
                    contextlib.redirect_stderr(stderr):
                code = synth.main(argv)
        evidence = (json.loads(Path(out).read_text())
                    if Path(out).exists() else None)
    return code, stdout.getvalue(), stderr.getvalue(), evidence


def verdicts(evidence):
    return {c["check"]: c["verdict"] for c in evidence["checks"]}


def detail(evidence, name):
    return next(c["detail"] for c in evidence["checks"]
                if c["check"] == name)


def make_fake(*, rooms, before, after, creator_name,
              patch_plan=None, get_plan=None):
    fake = FakeRest(rooms=rooms, guild_lists=[before, after],
                    creator_name=creator_name)
    fake.patch_plan = list(patch_plan or [])
    fake.get_plan = {path: list(plan)
                     for path, plan in (get_plan or {}).items()}
    return fake


def rename_cycle():
    """The exact creator PATCH pair of an applied-and-restored rename."""
    return [(CREATOR, {"name": TEMP_NAME}),
            (CREATOR, {"name": ORIGINAL_NAME})]


def room_round_trip():
    """The exact room PATCH pair of a verified limit round-trip."""
    return [(OWN, {"user_limit": PROBED_LIMIT}),
            (OWN, {"user_limit": INITIAL_LIMIT})]


class OwnedRoomBindingTest(unittest.TestCase):
    def test_unrelated_create_first_probes_only_the_owned_room(self):
        batches = [
            [GUILD_CREATE],
            [channel_create(FOREIGN), channel_create(OWN),
             voice_update(OWN)],
            [status_update(OWN)],
            [channel_create(REJOIN), voice_update(REJOIN)],
            [channel_delete(OWN), channel_delete(REJOIN)],
        ]
        rooms = {OWN: room_state(OWN),
                 FOREIGN: room_state(FOREIGN, name="kiln side room",
                                     position=64),
                 REJOIN: room_state(REJOIN, position=18)}
        before = [creator_state(TEMP_NAME)]
        after = [creator_state(TEMP_NAME), room_state(OWN),
                 room_state(REJOIN, position=18),
                 room_state(FOREIGN, name="kiln side room", position=64)]
        fake = make_fake(rooms=rooms, before=before, after=after,
                         creator_name=ORIGINAL_NAME)
        code, _, _, evidence = run_with_fakes(
            [], FakeSession(batches), fake)
        self.assertEqual(code, 0)
        self.assertEqual(evidence["room_id"], OWN)
        found = verdicts(evidence)
        self.assertEqual(found["create"], "PASS")
        self.assertEqual(found["moved"], "PASS")
        self.assertEqual(fake.patches,
                         [rename_cycle()[0]] + room_round_trip()
                         + [rename_cycle()[1]])

    def test_missing_move_refuses_before_any_patch(self):
        batches = [[GUILD_CREATE], [channel_create(FOREIGN)]]
        rooms = {FOREIGN: room_state(FOREIGN, position=64)}
        before = [creator_state(RESIDUE_NAME)]
        after = [creator_state(RESIDUE_NAME),
                 room_state(FOREIGN, position=64)]
        fake = make_fake(rooms=rooms, before=before, after=after,
                         creator_name=RESIDUE_NAME)
        code, _, _, evidence = run_with_fakes(
            [], FakeSession(batches), fake)
        self.assertEqual(code, 1)
        self.assertIsNone(evidence["room_id"])
        self.assertEqual(verdicts(evidence)["create"], "FAIL")
        self.assertEqual(fake.patches, [])

    def test_refused_room_still_restores_an_in_flight_creator_rename(self):
        batches = [[GUILD_CREATE], [channel_create(FOREIGN)]]
        rooms = {FOREIGN: room_state(FOREIGN, position=64)}
        before = [creator_state(TEMP_NAME)]
        after = [creator_state(TEMP_NAME),
                 room_state(FOREIGN, position=64)]
        fake = make_fake(rooms=rooms, before=before, after=after,
                         creator_name=ORIGINAL_NAME)
        code, _, _, evidence = run_with_fakes(
            [], FakeSession(batches), fake)
        self.assertEqual(code, 1)
        self.assertIsNone(evidence["room_id"])
        self.assertEqual(verdicts(evidence)["create"], "FAIL")
        self.assertEqual(fake.patches, rename_cycle())
        self.assertEqual(fake.rooms[CREATOR]["name"], ORIGINAL_NAME)

    def test_move_to_preexisting_room_refuses_before_any_patch(self):
        batches = [[GUILD_CREATE], [voice_update(PRE)]]
        rooms = {PRE: room_state(PRE, owner="777")}
        before = [creator_state(RESIDUE_NAME),
                  room_state(PRE, owner="777")]
        after = [creator_state(RESIDUE_NAME),
                 room_state(PRE, owner="777")]
        fake = make_fake(rooms=rooms, before=before, after=after,
                         creator_name=RESIDUE_NAME)
        code, _, _, evidence = run_with_fakes(
            [], FakeSession(batches), fake)
        self.assertEqual(code, 1)
        self.assertIsNone(evidence["room_id"])
        self.assertEqual(verdicts(evidence)["create"], "FAIL")
        self.assertEqual(fake.patches, [])

    def test_foreign_guild_events_never_bind_the_room(self):
        batches = [
            [GUILD_CREATE],
            [channel_create(FOREIGN, guild_id=OTHER_GUILD),
             voice_update(FOREIGN, guild_id=OTHER_GUILD),
             channel_create(OWN), voice_update(OWN)],
            [status_update(OWN)],
            [channel_create(REJOIN), voice_update(REJOIN)],
            [channel_delete(OWN), channel_delete(REJOIN)],
        ]
        rooms = {OWN: room_state(OWN),
                 FOREIGN: room_state(FOREIGN, name="kiln side room",
                                     position=64),
                 REJOIN: room_state(REJOIN, position=18)}
        before = [creator_state(TEMP_NAME)]
        after = [creator_state(TEMP_NAME), room_state(OWN),
                 room_state(REJOIN, position=18),
                 room_state(FOREIGN, name="kiln side room", position=64)]
        fake = make_fake(rooms=rooms, before=before, after=after,
                         creator_name=ORIGINAL_NAME)
        code, _, _, evidence = run_with_fakes(
            [], FakeSession(batches), fake)
        self.assertEqual(code, 0)
        self.assertEqual(evidence["room_id"], OWN)
        found = verdicts(evidence)
        self.assertEqual(found["create"], "PASS")
        self.assertEqual(found["moved"], "PASS")
        self.assertEqual(fake.patches,
                         [rename_cycle()[0]] + room_round_trip()
                         + [rename_cycle()[1]])

    def test_healthy_run_probes_the_owned_room_and_passes(self):
        batches = [
            [GUILD_CREATE],
            [channel_create(OWN), voice_update(OWN)],
            [status_update(OWN)],
            [channel_create(REJOIN), voice_update(REJOIN)],
            [channel_delete(OWN), channel_delete(REJOIN)],
        ]
        rooms = {OWN: room_state(OWN),
                 REJOIN: room_state(REJOIN, position=18)}
        before = [creator_state(TEMP_NAME)]
        after = [creator_state(TEMP_NAME), room_state(OWN),
                 room_state(REJOIN, position=18)]
        fake = make_fake(rooms=rooms, before=before, after=after,
                         creator_name=ORIGINAL_NAME)
        session = FakeSession(batches)
        code, _, _, evidence = run_with_fakes([], session, fake)
        self.assertEqual(code, 0)
        self.assertEqual(evidence["room_id"], OWN)
        found = verdicts(evidence)
        for name in ("create", "moved", "rejoin", "name", "position",
                     "owner", "status", "delete", "rename", "limit",
                     "privacy", "name_panel"):
            self.assertEqual(found[name], "PASS", name)
        self.assertEqual(found["metrics"], "SKIP")
        self.assertEqual(fake.patches,
                         [rename_cycle()[0]] + room_round_trip()
                         + [rename_cycle()[1]])
        self.assertEqual(session.voices, [(GUILD, CREATOR), (GUILD, CREATOR),
                                          (GUILD, None)])
        self.assertTrue(session.closed)

    def test_live_guild_fence_sends_nothing(self):
        fake = make_fake(rooms={}, before=[], after=[],
                         creator_name=ORIGINAL_NAME)
        code, _, stderr, evidence = run_with_fakes(
            ["--guild", synth.LIVE_GUILD_ID], FakeSession([]), fake)
        self.assertEqual(code, 2)
        self.assertIsNone(evidence)
        self.assertIn("REFUSED", stderr)
        self.assertEqual(fake.gets, [])
        self.assertEqual(fake.patches, [])


class RejoinCorrelationTest(unittest.TestCase):
    def run_rejoin_case(self, rejoin_batch, *, before_extra=None,
                        delete_extra=None):
        batches = [
            [GUILD_CREATE],
            [channel_create(OWN), voice_update(OWN)],
            [status_update(OWN)],
            list(rejoin_batch),
            [channel_delete(OWN)] + list(delete_extra or []),
        ]
        rooms = {OWN: room_state(OWN)}
        if before_extra:
            rooms[before_extra["id"]] = room_state(
                before_extra["id"], owner=before_extra.get("owner", ME),
                position=before_extra.get("position", 17))
        before = [creator_state(TEMP_NAME)]
        if before_extra:
            before.append(room_state(before_extra["id"],
                                     owner=before_extra.get("owner", ME),
                                     position=before_extra.get("position", 17)))
        after = [creator_state(TEMP_NAME), room_state(OWN)]
        fake = make_fake(rooms=rooms, before=before, after=after,
                         creator_name=ORIGINAL_NAME)
        return run_with_fakes([], FakeSession(batches), fake), fake

    def assert_first_room_intact(self, evidence, fake):
        self.assertEqual(evidence["room_id"], OWN)
        found = verdicts(evidence)
        self.assertEqual(found["create"], "PASS")
        self.assertEqual(found["moved"], "PASS")
        self.assertEqual(found["limit"], "PASS")
        self.assertEqual(fake.patches,
                         [rename_cycle()[0]] + room_round_trip()
                         + [rename_cycle()[1]])

    def test_foreign_guild_rejoin_does_not_count(self):
        (code, _, _, evidence), fake = self.run_rejoin_case(
            [voice_update(FOREIGN2, guild_id=OTHER_GUILD)],
            delete_extra=[channel_delete(FOREIGN2)])
        self.assertEqual(code, 1)
        self.assertEqual(verdicts(evidence)["rejoin"], "FAIL")
        self.assert_first_room_intact(evidence, fake)

    def test_move_to_preexisting_room_does_not_count_as_rejoin(self):
        (code, _, _, evidence), fake = self.run_rejoin_case(
            [voice_update(PRE)],
            before_extra={"id": PRE, "owner": "777"},
            delete_extra=[channel_delete(PRE)])
        self.assertEqual(code, 1)
        self.assertEqual(verdicts(evidence)["rejoin"], "FAIL")
        self.assert_first_room_intact(evidence, fake)

    def test_unrelated_channel_without_create_does_not_count_as_rejoin(self):
        (code, _, _, evidence), fake = self.run_rejoin_case(
            [voice_update(STRAY)],
            delete_extra=[channel_delete(STRAY)])
        self.assertEqual(code, 1)
        self.assertEqual(verdicts(evidence)["rejoin"], "FAIL")
        self.assert_first_room_intact(evidence, fake)


class UncertainLimitWriteTest(unittest.TestCase):
    def run_limit_case(self, *, patch_plan, get_plan=None,
                       creator_name=RESIDUE_NAME, full_run=False):
        if full_run:
            batches = [
                [GUILD_CREATE],
                [channel_create(OWN), voice_update(OWN)],
                [status_update(OWN)],
                [channel_create(REJOIN), voice_update(REJOIN)],
                [channel_delete(OWN), channel_delete(REJOIN)],
            ]
            rooms = {OWN: room_state(OWN),
                     REJOIN: room_state(REJOIN, position=18)}
            after = [creator_state(TEMP_NAME), room_state(OWN),
                     room_state(REJOIN, position=18)]
        else:
            batches = [
                [GUILD_CREATE],
                [channel_create(OWN), voice_update(OWN)],
                [], [], [],
            ]
            rooms = {OWN: room_state(OWN)}
            after = [creator_state(creator_name), room_state(OWN)]
        before = [creator_state(creator_name
                                if creator_name == RESIDUE_NAME
                                else TEMP_NAME)]
        if full_run:
            before = [creator_state(TEMP_NAME)]
        fake = make_fake(rooms=rooms, before=before, after=after,
                         creator_name=creator_name,
                         patch_plan=patch_plan, get_plan=get_plan)
        result = run_with_fakes([], FakeSession(batches), fake)
        return result, fake

    def assert_uncertain(self, evidence, detail_must_contain):
        found = verdicts(evidence)
        self.assertEqual(found["limit"], "FAIL")
        text = detail(evidence, "limit")
        self.assertIn(LIMIT_UNCERTAIN, text)
        self.assertIn(OWN, text)
        self.assertIn(str(INITIAL_LIMIT), text)
        for snippet in detail_must_contain:
            self.assertIn(snippet, text)

    def assert_bounded_cleanup(self, fake):
        self.assertEqual(fake.patches, room_round_trip())
        self.assertLessEqual(fake.gets_to(OWN), 8,
                             "cleanup must stay bounded: no retry loops")

    def test_lost_probe_response_restores_and_stays_failed(self):
        (code, _, _, evidence), fake = self.run_limit_case(patch_plan=[
            ("apply_then_raise",
             ConnectionError("simulated lost probe response")),
            "ok",
        ])
        self.assertEqual(code, 1)
        self.assert_uncertain(evidence, ["re-run"])
        self.assert_bounded_cleanup(fake)
        self.assertEqual(fake.rooms[OWN]["user_limit"], INITIAL_LIMIT)
        self.assertEqual(evidence["room_id"], OWN)

    def test_failed_probe_readback_restores_and_stays_failed(self):
        path = f"/channels/{OWN}"
        (code, _, _, evidence), fake = self.run_limit_case(
            patch_plan=["ok", "ok"],
            get_plan={path: ["ok",
                             ("raise", ConnectionError(
                                 "simulated read-back failure")),
                             "ok"]})
        self.assertEqual(code, 1)
        self.assert_uncertain(evidence, ["re-run"])
        self.assert_bounded_cleanup(fake)
        self.assertEqual(fake.rooms[OWN]["user_limit"], INITIAL_LIMIT)
        self.assertEqual(evidence["room_id"], OWN)

    def test_probe_readback_mismatch_restores_and_stays_failed(self):
        path = f"/channels/{OWN}"
        (code, _, _, evidence), fake = self.run_limit_case(
            patch_plan=["ok", "ok"],
            get_plan={path: ["ok", ("override", {"user_limit": 0}), "ok"]})
        self.assertEqual(code, 1)
        self.assert_uncertain(evidence, ["re-run"])
        self.assert_bounded_cleanup(fake)
        self.assertEqual(fake.rooms[OWN]["user_limit"], INITIAL_LIMIT)
        self.assertEqual(evidence["room_id"], OWN)

    def test_ambiguous_probe_status_restores_and_stays_failed(self):
        (code, _, _, evidence), fake = self.run_limit_case(
            patch_plan=[("status", 500), "ok"])
        self.assertEqual(code, 1)
        self.assert_uncertain(evidence, ["re-run"])
        self.assert_bounded_cleanup(fake)
        self.assertEqual(fake.rooms[OWN]["user_limit"], INITIAL_LIMIT)
        self.assertEqual(evidence["room_id"], OWN)

    def test_failed_cleanup_stays_failed_with_a_next_action(self):
        (code, stdout, stderr, evidence), fake = self.run_limit_case(
            patch_plan=[
                ("apply_then_raise",
                 ConnectionError("simulated lost probe response")),
                ("raise", ConnectionError("simulated restore failure")),
            ])
        self.assertEqual(code, 1)
        self.assert_uncertain(evidence, ["restore", "manually"])
        self.assertEqual(fake.patches, room_round_trip())
        self.assertEqual(fake.rooms[OWN]["user_limit"], PROBED_LIMIT)
        self.assertNotIn("Traceback", stderr)
        self.assertNotIn(FAKE_TOKEN, stdout + json.dumps(evidence))
        self.assertEqual(evidence["room_id"], OWN)

    def test_unverified_cleanup_readback_stays_failed_with_a_next_action(self):
        path = f"/channels/{OWN}"
        (code, _, _, evidence), fake = self.run_limit_case(
            patch_plan=[("apply_then_raise",
                         ConnectionError("simulated lost probe response")),
                        "ok"],
            get_plan={path: ["ok",
                             ("raise", ConnectionError(
                                 "simulated cleanup read-back failure"))]})
        self.assertEqual(code, 1)
        self.assert_uncertain(evidence, ["restore", "manually"])
        self.assertEqual(fake.patches, room_round_trip())
        self.assertEqual(evidence["room_id"], OWN)

    def test_rate_limited_probe_stays_skipped_without_restore(self):
        # The plan's first entry is the creator rename PATCH; the fault
        # targets the room probe PATCH that follows it.
        (code, _, _, evidence), fake = self.run_limit_case(
            patch_plan=["ok", ("status", 429)], creator_name=ORIGINAL_NAME,
            full_run=True)
        self.assertEqual(code, 0)
        self.assertEqual(verdicts(evidence)["limit"], "SKIP")
        self.assertEqual(fake.patches,
                         [rename_cycle()[0], (OWN, {"user_limit": PROBED_LIMIT}),
                          rename_cycle()[1]])
        self.assertEqual(fake.rooms[OWN]["user_limit"], INITIAL_LIMIT)

    def test_forbidden_probe_keeps_its_failure_without_restore(self):
        # The plan's first entry is the creator rename PATCH; the fault
        # targets the room probe PATCH that follows it.
        (code, _, _, evidence), fake = self.run_limit_case(
            patch_plan=["ok", ("status", 403)], creator_name=ORIGINAL_NAME,
            full_run=True)
        self.assertEqual(code, 1)
        found = verdicts(evidence)
        self.assertEqual(found["limit"], "FAIL")
        self.assertIn("403", detail(evidence, "limit"))
        self.assertEqual(fake.patches,
                         [rename_cycle()[0], (OWN, {"user_limit": PROBED_LIMIT}),
                          rename_cycle()[1]])
        self.assertEqual(fake.rooms[OWN]["user_limit"], INITIAL_LIMIT)


if __name__ == "__main__":
    unittest.main()
