"""Synthetic accounting events, not actual sends, SQL, history or runtime proof.

Actions is the suite's CI of record. The model observes inert declarations and
cannot enforce a sender, persist counters or authorize a rerun. No transport,
credential, database, live fixture or positive runtime entry point is exercised.
"""

import copy
from dataclasses import FrozenInstanceError
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_runtime_delivery import FeedDeliveryAccounting
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal


RUN = {"run_id": "1", "attempt": 1, "control_sha": "a" * 40}
KINDS = ("claim", "wire_attempt", "accepted", "committed", "replay")


def model(run=None):
    return FeedDeliveryAccounting(copy.deepcopy(RUN if run is None else run),
                                  "delivery-key", "item-nonce", "feed-post")


def event(kind):
    result = {"kind": kind, "run": copy.deepcopy(RUN), "key_ref": "delivery-key",
              "nonce_ref": "item-nonce", "claim_ref": "fresh-claim"}
    if kind in ("wire_attempt", "accepted"):
        result["operation_id"] = "feed-post"
    if kind in ("accepted", "committed", "replay"):
        result["message_id"] = "9201"
    return result


def advance(accounting, count):
    for kind in KINDS[:count]:
        accounting.observe(event(kind))


class DeliveryAccounting(unittest.TestCase):
    def test_single_fresh_attempt_commits_and_replay_never_adds_a_post(self):
        accounting = model()
        for index, kind in enumerate(KINDS):
            accounting.observe(event(kind))
            counts = accounting.counts()
            self.assertEqual(int(index >= 1), counts.attempted)
            self.assertEqual(int(index >= 2), counts.accepted)
            self.assertEqual(index >= 3, counts.committed)
            self.assertEqual(index == 4, counts.replay_observed)
            self.assertFalse(counts.adverse)
        self.assertIsNone(accounting.require_complete())
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_every_prefix_is_incomplete_until_replay(self):
        for count in range(len(KINDS)):
            with self.subTest(count=count):
                accounting = model()
                advance(accounting, count)
                with self.assertRaisesRegex(Refusal, "delivery_incomplete"):
                    accounting.require_complete()
                self.assertFalse(accounting.counts().adverse)

    def test_all_out_of_order_success_events_are_terminal(self):
        for position in range(len(KINDS) + 1):
            for kind in KINDS:
                if position < len(KINDS) and kind == KINDS[position]:
                    continue
                with self.subTest(position=position, kind=kind):
                    accounting = model()
                    advance(accounting, position)
                    with self.assertRaises(Refusal):
                        accounting.observe(event(kind))
                    self.assertTrue(accounting.counts().adverse)
                    with self.assertRaisesRegex(Refusal, "delivery_incomplete"):
                        accounting.require_complete()
                    with self.assertRaisesRegex(Refusal, "delivery_adverse"):
                        accounting.observe(event("claim"))

    def test_repost_is_counted_before_refusal_even_after_known_completion(self):
        for count in range(2, 6):
            with self.subTest(prefix=count):
                accounting = model()
                advance(accounting, count)
                with self.assertRaisesRegex(Refusal, "delivery_repost"):
                    accounting.observe(event("wire_attempt"))
                self.assertEqual(2, accounting.counts().attempted)
                self.assertTrue(accounting.counts().adverse)

    def test_failure_and_unknown_events_never_reopen_or_erase_a_claim(self):
        failures = ("prewire_failure", "wire_failure", "completion_unknown",
                    "history_unknown", "abort")
        for count in range(1, 6):
            for kind in failures:
                with self.subTest(prefix=count, failure=kind):
                    accounting = model()
                    advance(accounting, count)
                    before = accounting.counts()
                    with self.assertRaisesRegex(Refusal, "delivery_uncertain"):
                        accounting.observe(event(kind))
                    after = accounting.counts()
                    self.assertEqual(before.attempted, after.attempted)
                    self.assertEqual(before.accepted, after.accepted)
                    self.assertEqual(before.committed, after.committed)
                    self.assertTrue(after.adverse)
                    for subsequent in ("wire_attempt", "claim", "replay", "committed"):
                        with self.assertRaisesRegex(Refusal, "delivery_adverse"):
                            accounting.observe(event(subsequent))
                    with self.assertRaisesRegex(Refusal, "delivery_incomplete"):
                        accounting.require_complete()

    def test_timeout_does_not_mean_zero_attempts_or_permission_to_retry(self):
        accounting = model()
        advance(accounting, 2)
        with self.assertRaises(Refusal):
            accounting.observe(event("wire_failure"))
        counts = accounting.counts()
        self.assertEqual((1, 0, False, False, True),
                         (counts.attempted, counts.accepted, counts.committed,
                          counts.replay_observed, counts.adverse))
        with self.assertRaises(Refusal):
            accounting.observe(event("wire_attempt"))

    def test_completion_ambiguity_retains_the_known_accepted_message_count(self):
        accounting = model()
        advance(accounting, 3)
        with self.assertRaises(Refusal):
            accounting.observe(event("completion_unknown"))
        self.assertEqual(1, accounting.counts().attempted)
        self.assertEqual(1, accounting.counts().accepted)
        self.assertFalse(accounting.counts().committed)
        self.assertTrue(accounting.counts().adverse)

    def test_wrong_run_attempt_control_key_nonce_or_claim_is_terminal(self):
        changes = [lambda e: e["run"].update(run_id="2"),
                   lambda e: e["run"].update(attempt=2),
                   lambda e: e["run"].update(control_sha="b" * 40),
                   lambda e: e.update(key_ref="other-key"),
                   lambda e: e.update(nonce_ref="other-nonce"),
                   lambda e: e.update(claim_ref="other-claim")]
        for position, kind in enumerate(KINDS[1:], 1):
            for index, change in enumerate(changes):
                with self.subTest(kind=kind, mutation=index):
                    accounting = model()
                    advance(accounting, position)
                    value = event(kind)
                    change(value)
                    with self.assertRaises(Refusal):
                        accounting.observe(value)
                    self.assertTrue(accounting.counts().adverse)

    def test_acceptance_and_wire_operation_ids_are_pinned(self):
        for position, kind in ((1, "wire_attempt"), (2, "accepted")):
            with self.subTest(kind=kind):
                accounting = model()
                advance(accounting, position)
                value = event(kind)
                value["operation_id"] = "another-post"
                with self.assertRaises(Refusal):
                    accounting.observe(value)
                self.assertTrue(accounting.counts().adverse)

    def test_commit_and_replay_require_the_same_accepted_message(self):
        for position, kind in ((3, "committed"), (4, "replay")):
            with self.subTest(kind=kind):
                accounting = model()
                advance(accounting, position)
                value = event(kind)
                value["message_id"] = "9202"
                with self.assertRaises(Refusal):
                    accounting.observe(value)
                self.assertTrue(accounting.counts().adverse)

    def test_message_id_is_a_canonical_nonzero_u64(self):
        for identifier in ("0", "01", "18446744073709551616", 9201, True, "-1", "9.2"):
            with self.subTest(identifier=identifier):
                accounting = model()
                advance(accounting, 2)
                value = event("accepted")
                value["message_id"] = identifier
                with self.assertRaises(Refusal):
                    accounting.observe(value)
                self.assertTrue(accounting.counts().adverse)
        accounting = model()
        for kind in KINDS:
            value = event(kind)
            if "message_id" in value:
                value["message_id"] = "18446744073709551615"
            accounting.observe(value)
        accounting.require_complete()

    def test_unknown_recovery_cleanup_and_reset_events_cannot_clear_adversity(self):
        for kind in ("recover", "history_found", "release_unposted", "reset", "cleanup",
                     "restart", "retry", "pass", "", True, None):
            with self.subTest(kind=kind):
                accounting = model()
                advance(accounting, 2)
                with self.assertRaises(Refusal):
                    accounting.observe(event(kind))
                self.assertTrue(accounting.counts().adverse)
                with self.assertRaises(Refusal):
                    accounting.require_complete()

    def test_events_are_closed_and_missing_or_extra_fields_poison_the_model(self):
        changes = [lambda e: e.update(extra=True), lambda e: e.pop("nonce_ref"),
                   lambda e: e["run"].update(extra=True),
                   lambda e: e["run"].update(attempt=True),
                   lambda e: e.update(claim_ref="unknown"),
                   lambda e: e.update(key_ref="a" * 4097)]
        for index, change in enumerate(changes):
            with self.subTest(mutation=index):
                accounting = model()
                value = event("claim")
                change(value)
                with self.assertRaises(Refusal):
                    accounting.observe(value)
                self.assertTrue(accounting.counts().adverse)

    def test_nonobject_event_is_terminal(self):
        for value in (None, [], "claim", 1, True):
            with self.subTest(value=value):
                accounting = model()
                with self.assertRaises(Refusal):
                    accounting.observe(value)
                self.assertTrue(accounting.counts().adverse)

    def test_run_input_and_events_are_not_retained_by_reference(self):
        run = copy.deepcopy(RUN)
        accounting = FeedDeliveryAccounting(run, "delivery-key", "item-nonce", "feed-post")
        run["attempt"] = 2
        value = event("claim")
        accounting.observe(value)
        value["run"]["attempt"] = 2
        value["claim_ref"] = "other-claim"
        for kind in KINDS[1:]:
            accounting.observe(event(kind))
        accounting.require_complete()
        self.assertFalse(accounting.counts().adverse)

    def test_snapshot_is_immutable_and_does_not_carry_raw_claims_or_messages(self):
        accounting = model()
        advance(accounting, 3)
        counts = accounting.counts()
        with self.assertRaises(FrozenInstanceError):
            counts.attempted = 0
        self.assertEqual({"attempted", "accepted", "committed", "replay_observed", "adverse"},
                         set(vars(counts)))
        self.assertEqual(1, accounting.counts().attempted)

    def test_constructor_refuses_unbound_and_malformed_scope(self):
        for value in (None, {}, {**RUN, "attempt": True}, {**RUN, "extra": "value"}):
            with self.subTest(run=value), self.assertRaises(Refusal):
                model(value) if value is not None else FeedDeliveryAccounting(
                    None, "delivery-key", "item-nonce", "feed-post")
        for values in (("unknown", "item-nonce", "feed-post"),
                       ("delivery-key", "all", "feed-post"),
                       ("delivery-key", "item-nonce", "")):
            with self.subTest(refs=values), self.assertRaises(Refusal):
                FeedDeliveryAccounting(copy.deepcopy(RUN), *values)


if __name__ == "__main__":
    unittest.main()
