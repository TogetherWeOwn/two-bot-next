#!/usr/bin/env python3
"""Acceptance tests: smoke-honesty edge classifier never crashes on hostile input.

Protected acceptance file: the implementation change must satisfy these
tests without editing them. Test defects are corrected here, not by
weakening assertions in the build.

Contract under test (``scripts/staging_edge_cases.py``):

* ``classify_observation`` maps every hostile observation to
  ``generic_failure`` with zero raise: non-dict observations, and
  non-string truthy ``reply``/``reason`` values (int, float, bool,
  bytes, list, dict, tuple, set) -- including containers that hold an
  edge surface as a member, which must NOT match that edge arm.
* ``classify_pack`` turns malformed packs (non-dict, missing or
  non-list ``cases``) into a fixed ``ValueError`` refusal naming
  ``cases``, and malformed cases (non-dict, missing ``name``) into a
  fixed ``ValueError`` refusal naming the ``case`` -- never a raw
  ``KeyError``/``TypeError``/``AttributeError``/``IndexError``.
* Refusal text never echoes hostile payload bytes.
* Valid edge shapes classify exactly as today: the four fixture
  outcomes, the explicit pinned surfaces, lenient missing-observation
  rows, and the ``None``-means-default-pack load path.

``classify_pack(None)`` loads the default fixture pack; that is the
documented default, not a malformed pack, and is pinned as a control.

Offline only: no network, no tokens, no guild touch.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_staging_edge_cases_smoke_honesty_acceptance.py' -v
"""

import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_edge_cases as edge

# A unique marker, readable on purpose so a leak is unmistakable and no
# credential-shaped literal is stored anywhere in this file.
SENTINEL = "EdgeCaseNeverEcho7Q"

TRUTHY_NON_STRINGS = (123, -1, 4.5, True, b"reply-bytes", ["not", "a", "string"],
                      {"text": "x"}, ("x",), {"x"})


class HostileObservationTests(unittest.TestCase):
    def setUp(self):
        # Any real HTTP is a test failure: the classifier never fetches.
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)

    def test_non_dict_observations_classify_generic_failure(self):
        for observation in (None, 0, 123, 4.5, True, "reply text", b"reply-bytes",
                            [], ["x"], (), ("x",), {1, 2}, {edge.UNRANKED_LABEL}):
            with self.subTest(observation=repr(observation)):
                self.assertEqual(edge.classify_observation(observation),
                                 edge.GENERIC_FAILURE)

    def test_non_string_truthy_reply_classifies_generic_failure(self):
        for reply in TRUTHY_NON_STRINGS:
            with self.subTest(reply=repr(reply)):
                self.assertEqual(
                    edge.classify_observation({"reply": reply, "reason": None}),
                    edge.GENERIC_FAILURE)

    def test_reply_containers_holding_edge_surfaces_do_not_match(self):
        # Membership against a container must not trip an edge arm: only a
        # genuine string surface classifies away from generic_failure.
        hostile = ([edge.UNRANKED_LABEL], (edge.UNRANKED_LABEL,),
                   {edge.UNRANKED_LABEL: True}, [edge.MANAGE_SERVER_DENIAL])
        for reply in hostile:
            with self.subTest(reply=repr(reply)):
                self.assertEqual(edge.classify_observation({"reply": reply}),
                                 edge.GENERIC_FAILURE)

    def test_non_string_truthy_reason_classifies_generic_failure(self):
        hostile = (*TRUTHY_NON_STRINGS, [edge.TIMEOUT_REASON_PREFIX + "2.0s)"],
                   {"reason": edge.TIMEOUT_REASON_PREFIX})
        for reason in hostile:
            with self.subTest(reason=repr(reason)):
                self.assertEqual(
                    edge.classify_observation({"reply": None, "reason": reason}),
                    edge.GENERIC_FAILURE)

    def test_combined_hostile_reply_and_reason_classify_generic_failure(self):
        for observation in ({"reply": 123, "reason": 456},
                            {"reply": ["x"], "reason": {"y": 1}},
                            {"reply": b"x", "reason": (edge.TIMEOUT_REASON_PREFIX,)},
                            {"reply": {"k": "v"}, "reason": ["r"]}):
            with self.subTest(observation=repr(observation)):
                self.assertEqual(edge.classify_observation(observation),
                                 edge.GENERIC_FAILURE)

    def test_falsy_non_string_values_still_coerce_to_generic_failure(self):
        for value in (0, 0.0, False, [], {}, (), set(), "", None):
            with self.subTest(value=repr(value)):
                self.assertEqual(
                    edge.classify_observation({"reply": value, "reason": value}),
                    edge.GENERIC_FAILURE)


class HostilePackTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)

    def assert_fixed_cases_refusal(self, pack, label):
        with self.assertRaises(ValueError, msg=f"no fixed cases refusal in {label}") as caught:
            edge.classify_pack(pack)
        self.assertIn("cases", str(caught.exception),
                      f"pack refusal does not name cases in {label}")
        self.assertNotIn(SENTINEL, str(caught.exception),
                         f"pack refusal echoes hostile bytes in {label}")

    def assert_fixed_case_refusal(self, pack, label):
        with self.assertRaises(ValueError, msg=f"no fixed case refusal in {label}") as caught:
            edge.classify_pack(pack)
        self.assertIn("case", str(caught.exception).lower(),
                      f"case refusal does not name the case in {label}")
        self.assertNotIn(SENTINEL, str(caught.exception),
                         f"case refusal echoes hostile bytes in {label}")

    def test_packs_without_cases_are_fixed_refusals(self):
        for pack in ({}, {"version": 1}, {"version": 1, "evil": SENTINEL}):
            with self.subTest(pack=repr(pack)):
                self.assert_fixed_cases_refusal(pack, repr(pack))

    def test_non_dict_packs_are_fixed_refusals(self):
        for pack in ([], "pack", 123, 4.5, True, b"pack"):
            with self.subTest(pack=repr(pack)):
                self.assert_fixed_cases_refusal(pack, repr(pack))

    def test_non_list_cases_are_fixed_refusals(self):
        for cases in (None, "ab", 123, {"a": 1}, SENTINEL):
            with self.subTest(cases=repr(cases)):
                self.assert_fixed_cases_refusal({"cases": cases}, repr(cases))

    def test_cases_without_name_are_fixed_refusals(self):
        for case in ({}, {"observation": {"reply": "x"}},
                     {"observation": {}, "evil": SENTINEL}):
            with self.subTest(case=repr(case)):
                self.assert_fixed_case_refusal({"cases": [case]}, repr(case))

    def test_non_dict_cases_are_fixed_refusals(self):
        for case in (123, "x", None, ["name"], SENTINEL):
            with self.subTest(case=repr(case)):
                self.assert_fixed_case_refusal({"cases": [case]}, repr(case))

    def test_cases_with_hostile_observations_classify_generic_rows(self):
        pack = {"cases": [
            {"name": "hostile-reply", "observation": {"reply": 123},
             "expected_outcome": "generic_failure"},
            {"name": "hostile-reason", "observation": {"reason": ["r"]},
             "expected_outcome": "generic_failure"},
            {"name": "hostile-both", "observation": {"reply": 1, "reason": 2},
             "expected_outcome": "generic_failure"},
        ]}
        self.assertEqual(
            edge.classify_pack(pack),
            [(case["name"], edge.GENERIC_FAILURE, "generic_failure")
             for case in pack["cases"]])


class ValidShapeControls(unittest.TestCase):
    def setUp(self):
        guard = mock.patch("urllib.request.OpenerDirector.open",
                           side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        self.pack = edge.load_pack()

    def test_fixture_pack_classifies_exactly_as_today(self):
        self.assertEqual(
            {name: outcome for name, outcome, _ in edge.classify_pack(self.pack)},
            {"permission_denied": edge.PERMISSION_DENIED,
             "interaction_timeout": edge.INTERACTION_TIMEOUT,
             "empty_rank": edge.EMPTY_RANK,
             "generic_failure_contrast": edge.GENERIC_FAILURE})

    def test_explicit_valid_surfaces_classify_exactly_as_today(self):
        self.assertEqual(edge.classify_observation({"reply": edge.MANAGE_SERVER_DENIAL}),
                         edge.PERMISSION_DENIED)
        self.assertEqual(
            edge.classify_observation(
                {"reply": None, "reason": edge.TIMEOUT_REASON_PREFIX + "2.0s)"}),
            edge.INTERACTION_TIMEOUT)
        # The timeout arm is a prefix match, not a full shape match.
        self.assertEqual(edge.classify_observation({"reason": edge.TIMEOUT_REASON_PREFIX}),
                         edge.INTERACTION_TIMEOUT)
        self.assertEqual(
            edge.classify_observation({"reply": "noise " + edge.UNRANKED_LABEL + " noise"}),
            edge.EMPTY_RANK)
        self.assertEqual(
            edge.classify_observation(
                {"reply": edge.GENERIC_FAILURE_PREFIX + "ref deadbeef) please retry"}),
            edge.GENERIC_FAILURE)
        self.assertEqual(edge.classify_observation({}), edge.GENERIC_FAILURE)
        self.assertEqual(edge.classify_observation({"reply": "", "reason": ""}),
                         edge.GENERIC_FAILURE)

    def test_missing_observations_stay_lenient_generic_rows(self):
        pack = {"cases": [
            {"name": "no-observation-key"},
            {"name": "none-observation", "observation": None},
            {"name": "non-dict-observation", "observation": 123},
        ]}
        self.assertEqual(
            edge.classify_pack(pack),
            [(case["name"], edge.GENERIC_FAILURE, None) for case in pack["cases"]])

    def test_empty_cases_list_stays_valid(self):
        self.assertEqual(edge.classify_pack({"cases": []}), [])

    def test_none_loads_the_default_pack(self):
        self.assertEqual(edge.classify_pack(None), edge.classify_pack(self.pack))
        self.assertEqual(edge.classify_pack(), edge.classify_pack(self.pack))


if __name__ == "__main__":
    unittest.main()
