"""Hermetic delivery-trace storage adversaries; not SQL, stored-row or send proof.

GitHub Actions is the suite's CI of record; nothing here was run locally unless
disclosed separately. The mandatory feed_delivery_trace.storage object carries
fabricated full column projections (claim token as a digest only) that two
resource_state sidecars pin by canonical digest. Passing shows only that these
fabricated rows, states and transitions agree with the fabricated ledgers. The
graph stays PARTIAL: no ownership or authority chain, claim-token fencing, SQL
result or history read is modeled, and runtime_preflight stays unsupported.

Semantic negatives repin every derived hash, including the full-row state
digests, so the storage checker refuses on meaning rather than a stale pin; only
the dedicated stale tests pass storage=False. Where an earlier predicate (schema,
ordering, upstream case/telemetry check) may refuse first, the test asserts only
a Refusal.
"""

import copy
from datetime import datetime, timedelta, timezone
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_delivery_trace_fixtures import (
    CLAIM_TOKEN_SHA256, READ_TRANSACTION, TRACE, claim_state_ref, completion_state_ref,
    delivery_effect, delivery_entry, delivery_trace_fixture, repin_delivery_trace,
)
from announcement_feed_exchange_fixtures import EXCHANGE, feed_exchange_fixture
from announcement_provenance_fixtures import evidence_index
from announcement_runtime_delivery_trace import check_delivery_trace_claims
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import REFERENCE_LIMIT, Refusal

NULL = {"kind": "sql_null"}
COLUMNS = ("feed_id", "item_key", "nonce", "first_seen_at", "state", "message_id",
           "delivered_at", "claim_token_sha256", "claimed_at")
ROWS = ("claimed", "completed", "replay")
FORMAT = "%Y-%m-%dT%H:%M:%SZ"


def storage_of(records):
    return records[TRACE]["data"]["storage"]


def shift(stamp, seconds):
    moved = datetime.strptime(stamp, FORMAT).replace(tzinfo=timezone.utc) + timedelta(seconds=seconds)
    return moved.strftime(FORMAT)


def transition(receipt, records, method):
    ref = delivery_effect(receipt, method)["db_transition_refs"][0]
    return records[ref]["data"]["claim"]


def set_times(records, field, value, rows=ROWS):
    for name in rows:
        storage_of(records)[name][field] = value


class Storage(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_delivery_trace_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, **repin):
        receipt, records, now = delivery_trace_fixture()
        mutate(receipt, records)
        repin_delivery_trace(receipt, records, **repin)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        if code is not None:
            self.assertEqual(code, caught.exception.code)

    def accept(self, mutate, **repin):
        receipt, records, now = delivery_trace_fixture()
        mutate(receipt, records)
        repin_delivery_trace(receipt, records, **repin)
        self.assertIsNone(self.check(receipt, records, now))


class Golden(Storage):
    def test_baseline_storage_is_exact_and_bounded(self):
        receipt, records, now = delivery_trace_fixture()
        storage = storage_of(records)
        key = records["key-delivery"]["data"]
        nonce = records["selected-item"]["data"]["nonce"]
        message = records[EXCHANGE]["data"]["response"]["message"]["id"]
        claim_at = delivery_effect(receipt, "db_insert")["observed_at"]
        done_at = delivery_effect(receipt, "db_cas")["observed_at"]
        self.assertEqual({"claim_token_sha256", "claimed", "completed", "replay"}, set(storage))
        self.assertEqual(CLAIM_TOKEN_SHA256, storage["claim_token_sha256"])
        self.assertEqual({
            "feed_id": key["feed_id"], "item_key": key["item_key"], "nonce": nonce,
            "first_seen_at": claim_at, "state": "pending", "message_id": NULL,
            "delivered_at": NULL, "claim_token_sha256": CLAIM_TOKEN_SHA256,
            "claimed_at": claim_at}, storage["claimed"])
        self.assertEqual({
            "feed_id": key["feed_id"], "item_key": key["item_key"], "nonce": nonce,
            "first_seen_at": claim_at, "state": "delivered", "message_id": message,
            "delivered_at": done_at, "claim_token_sha256": NULL, "claimed_at": NULL},
            storage["completed"])
        self.assertEqual(storage["completed"], storage["replay"])
        self.assertIsNot(storage["completed"], storage["replay"])
        for name in ROWS:
            self.assertEqual(set(COLUMNS), set(storage[name]))
        self.assertLessEqual(len(records), REFERENCE_LIMIT)
        self.assertIsNone(self.check(receipt, records, now))

    def test_record_addition_is_four_sidecars_over_the_exchange_fixture(self):
        _, base, _ = feed_exchange_fixture()
        receipt, records, _ = delivery_trace_fixture()
        self.assertEqual({TRACE, READ_TRANSACTION, claim_state_ref(receipt),
                          completion_state_ref(receipt)}, set(records) - set(base))
        self.assertEqual(4, len(records) - len(base))

    def test_pending_and_completed_states_pin_the_full_rows(self):
        receipt, records, _ = delivery_trace_fixture()
        entry = delivery_entry(receipt)
        observer = receipt["observations"]["coverage"][0]["observer_ref"]
        storage = storage_of(records)
        for ref, version, row, method in (
                (claim_state_ref(receipt), "1", storage["claimed"], "db_insert"),
                (completion_state_ref(receipt), "2", storage["completed"], "db_cas")):
            data = records[ref]["data"]
            self.assertEqual("resource_state", records[ref]["kind"])
            self.assertEqual({"ref": ref, "presence": "present", "version": version,
                              "value_sha256": canonical_digest(row)}, data["claim"])
            self.assertEqual(entry["ref"], data["resource_ref"])
            self.assertEqual(canonical_digest(entry["identity"]), data["identity_sha256"])
            self.assertEqual(entry["owner"], data["owner_ref"])
            self.assertEqual(observer, data["observer_ref"])
            self.assertEqual(delivery_effect(receipt, method)["observed_at"], data["observed_at"])
        # The replay row is the completed row, so no third state exists.
        self.assertEqual(storage["completed"], storage["replay"])

    def test_state_refs_are_unique_and_not_baseline_or_final(self):
        receipt, _, _ = delivery_trace_fixture()
        entry = delivery_entry(receipt)
        refs = [entry["before_state_ref"], claim_state_ref(receipt),
                completion_state_ref(receipt), entry["after_state_ref"]]
        self.assertEqual(4, len(set(refs)))

    def test_transitions_chain_baseline_pending_delivered_cleanup(self):
        receipt, records, _ = delivery_trace_fixture()
        entry = delivery_entry(receipt)
        pending, delivered = claim_state_ref(receipt), completion_state_ref(receipt)
        claim = transition(receipt, records, "db_insert")
        done = transition(receipt, records, "db_cas")
        cleanup = transition(receipt, records, "db_delete_owned")
        self.assertEqual((entry["before_state_ref"], pending, pending),
                         (claim["before_state_ref"], claim["intended_after_state_ref"],
                          claim["actual_after_state_ref"]))
        self.assertEqual((pending, delivered, delivered),
                         (done["before_state_ref"], done["intended_after_state_ref"],
                          done["actual_after_state_ref"]))
        self.assertEqual(delivered, cleanup["before_state_ref"])
        self.assertEqual(entry["after_state_ref"], cleanup["actual_after_state_ref"])
        self.assertEqual(entry["after_state_ref"], cleanup["intended_after_state_ref"])

    def test_runtime_admission_remains_unconditional_hold(self):
        receipt, _, _ = delivery_trace_fixture()
        self.assertFalse(receipt["promotable"])
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_checker_does_not_mutate_receipt_or_sidecars(self):
        receipt, records, now = delivery_trace_fixture()
        before = copy.deepcopy((receipt, records))
        self.assertIsNone(self.check(receipt, records, now))
        self.assertEqual(before, (receipt, records))

    def test_fixture_calls_are_equal_but_independent(self):
        first, second = delivery_trace_fixture(), delivery_trace_fixture()
        self.assertEqual(first, second)
        storage_of(first[1])["claimed"]["nonce"] = "0" * 24
        self.assertEqual(second, delivery_trace_fixture())


class Shape(Storage):
    def test_missing_storage_or_members_refuse(self):
        cases = [("storage", lambda r, rec: rec[TRACE]["data"].pop("storage"))]
        cases += [("storage." + name, lambda r, rec, name=name: storage_of(rec).pop(name))
                  for name in ("claim_token_sha256", "claimed", "completed", "replay")]
        for row in ROWS:
            cases += [("%s.%s" % (row, column),
                       lambda r, rec, row=row, column=column: storage_of(rec)[row].pop(column))
                      for column in COLUMNS]
        for name, mutate in cases:
            with self.subTest(name):
                self.reject(mutate)

    def test_extra_fields_refuse(self):
        self.reject(lambda r, rec: storage_of(rec).update(extra=1))
        self.reject(lambda r, rec: storage_of(rec).update(alternate=copy.deepcopy(storage_of(rec)["replay"])))
        for row in ROWS:
            with self.subTest(row):
                self.reject(lambda r, rec, row=row: storage_of(rec)[row].update(extra=1))
        with self.subTest("raw claim token"):
            self.reject(lambda r, rec: storage_of(rec)["claimed"].update(claim_token="token"))

    def test_nullable_and_wrong_typed_storage_refuses(self):
        self.reject(lambda r, rec: rec[TRACE]["data"].update(storage=None))
        for name in ("claim_token_sha256", "claimed", "completed", "replay"):
            with self.subTest("null " + name):
                self.reject(lambda r, rec, name=name: storage_of(rec).update({name: None}))
        for row in ROWS:
            for column in COLUMNS:
                with self.subTest("null %s.%s" % (row, column)):
                    self.reject(lambda r, rec, row=row, column=column:
                                storage_of(rec)[row].update({column: None}))
        with self.subTest("python None is not the typed SQL NULL marker"):
            self.reject(lambda r, rec: storage_of(rec)["claimed"].update(message_id=None))
        with self.subTest("SQL NULL marker with extra field"):
            self.reject(lambda r, rec: storage_of(rec)["claimed"]["message_id"].update(extra=1))
        with self.subTest("SQL NULL marker of another kind"):
            self.reject(lambda r, rec: storage_of(rec)["claimed"].update(message_id={"kind": "null"}))
        with self.subTest("SQL NULL where a value is required"):
            self.reject(lambda r, rec: storage_of(rec)["completed"].update(message_id=copy.deepcopy(NULL)))
            self.reject(lambda r, rec: storage_of(rec)["completed"].update(delivered_at=copy.deepcopy(NULL)))
            self.reject(lambda r, rec: storage_of(rec)["claimed"].update(claimed_at=copy.deepcopy(NULL)))
            self.reject(lambda r, rec: storage_of(rec)["claimed"].update(
                claim_token_sha256=copy.deepcopy(NULL)))

    def test_wrong_types_and_formats_refuse(self):
        mutations = {
            "storage list": lambda r, rec: rec[TRACE]["data"].update(storage=[]),
            "storage string": lambda r, rec: rec[TRACE]["data"].update(storage="rows"),
            "row list": lambda r, rec: storage_of(rec).update(claimed=[]),
            "token uppercase": lambda r, rec: storage_of(rec).update(
                claim_token_sha256=CLAIM_TOKEN_SHA256.upper()),
            "token short": lambda r, rec: storage_of(rec).update(
                claim_token_sha256=CLAIM_TOKEN_SHA256[:-1]),
            "token integer": lambda r, rec: storage_of(rec).update(claim_token_sha256=1),
            "row token not hex": lambda r, rec: storage_of(rec)["claimed"].update(
                claim_token_sha256="z" * 64),
            "feed id integer": lambda r, rec: storage_of(rec)["claimed"].update(feed_id=1),
            "feed id empty": lambda r, rec: storage_of(rec)["claimed"].update(feed_id=""),
            "item key slash": lambda r, rec: storage_of(rec)["claimed"].update(item_key="a/b"),
            "nonce short": lambda r, rec: storage_of(rec)["claimed"].update(nonce="abc"),
            "nonce integer": lambda r, rec: storage_of(rec)["claimed"].update(nonce=1),
            "first seen not utc": lambda r, rec: storage_of(rec)["claimed"].update(
                first_seen_at="2026-01-01 00:08:00"),
            "claimed at integer": lambda r, rec: storage_of(rec)["claimed"].update(claimed_at=1),
            "delivered at not utc": lambda r, rec: storage_of(rec)["completed"].update(
                delivered_at="yesterday"),
            "message id integer": lambda r, rec: storage_of(rec)["completed"].update(message_id=9201),
            "message id zero": lambda r, rec: storage_of(rec)["completed"].update(message_id="0"),
        }
        for name, mutate in mutations.items():
            with self.subTest(name):
                self.reject(mutate)

    def test_unsupported_and_misplaced_states_refuse(self):
        for state in ("unknown", "abandoned", "claimed", "PENDING", "", None, 1):
            for row in ROWS:
                with self.subTest("%s=%r" % (row, state)):
                    self.reject(lambda r, rec, row=row, state=state:
                                storage_of(rec)[row].update(state=state))
        with self.subTest("claimed row is delivered"):
            self.reject(lambda r, rec: storage_of(rec).update(
                claimed=copy.deepcopy(storage_of(rec)["completed"])))
        with self.subTest("completed row is pending"):
            self.reject(lambda r, rec: storage_of(rec).update(
                completed=copy.deepcopy(storage_of(rec)["claimed"])))
        with self.subTest("replay row is pending"):
            self.reject(lambda r, rec: storage_of(rec).update(
                replay=copy.deepcopy(storage_of(rec)["claimed"])))


class Rows(Storage):
    def test_literal_key_columns_must_match_the_delivery_key(self):
        for row in ROWS:
            with self.subTest(row + " feed"):
                self.reject(lambda r, rec, row=row: storage_of(rec)[row].update(feed_id="other-relay"))
            with self.subTest(row + " item"):
                self.reject(lambda r, rec, row=row: storage_of(rec)[row].update(item_key="item-two"))

    def test_nonce_must_match_the_selection_in_every_row(self):
        for row in ROWS:
            with self.subTest(row):
                self.reject(lambda r, rec, row=row: storage_of(rec)[row].update(nonce="0" * 24))

    def test_claim_token_digest_binds_the_claimed_row_only(self):
        other = "1" * 64
        self.reject(lambda r, rec: storage_of(rec).update(claim_token_sha256=other))
        self.reject(lambda r, rec: storage_of(rec)["claimed"].update(claim_token_sha256=other))

    def test_completion_must_clear_token_and_claim_time(self):
        for row in ("completed", "replay"):
            with self.subTest(row + " retains token"):
                self.reject(lambda r, rec, row=row: storage_of(rec)[row].update(
                    claim_token_sha256=CLAIM_TOKEN_SHA256))
            with self.subTest(row + " retains claimed_at"):
                self.reject(lambda r, rec, row=row: storage_of(rec)[row].update(
                    claimed_at=storage_of(rec)["claimed"]["claimed_at"]))

    def test_first_seen_is_immutable_across_rows(self):
        for row in ("completed", "replay"):
            with self.subTest(row):
                self.reject(lambda r, rec, row=row: storage_of(rec)[row].update(
                    first_seen_at=shift(storage_of(rec)[row]["first_seen_at"], 1)))
        self.reject(lambda r, rec: storage_of(rec)["claimed"].update(
            first_seen_at=shift(storage_of(rec)["claimed"]["first_seen_at"], -1)))

    def test_message_must_match_the_exchange_response(self):
        for row in ("completed", "replay"):
            with self.subTest(row):
                self.reject(lambda r, rec, row=row: storage_of(rec)[row].update(message_id="9999"))

        def all_rows(receipt, records):
            for row in ("completed", "replay"):
                storage_of(records)[row]["message_id"] = "9999"
        self.reject(all_rows)

    def test_replay_must_be_identical_to_completed(self):
        self.reject(lambda r, rec: storage_of(rec)["replay"].update(
            delivered_at=shift(storage_of(rec)["replay"]["delivered_at"], 1)))
        self.reject(lambda r, rec: storage_of(rec)["replay"].update(message_id="9999"))
        self.reject(lambda r, rec: storage_of(rec)["replay"].update(nonce="0" * 24))

    def test_expected_rows_are_derived_from_claimed_and_delivered_columns(self):
        # Moving claimed_at alone, or delivered_at alone, in only one row diverges
        # from the row expected from storage.claimed / storage.completed.
        self.reject(lambda r, rec: storage_of(rec)["claimed"].update(
            claimed_at=shift(storage_of(rec)["claimed"]["claimed_at"], -1)))
        self.reject(lambda r, rec: storage_of(rec)["replay"].update(
            delivered_at=shift(storage_of(rec)["replay"]["delivered_at"], -1)))


class Timestamps(Storage):
    def bounds(self, receipt, records):
        entry = delivery_entry(receipt)
        baseline = records[entry["before_state_ref"]]["data"]["observed_at"]
        claim = delivery_effect(receipt, "db_insert")["observed_at"]
        received = records[EXCHANGE]["data"]["response"]["received_at"]
        done = delivery_effect(receipt, "db_cas")["observed_at"]
        return baseline, claim, received, done

    def claim_columns(self, records, value):
        set_times(records, "first_seen_at", value)
        storage_of(records)["claimed"]["claimed_at"] = value

    def test_claim_columns_before_the_baseline_or_after_the_claim_refuse(self):
        receipt, records, _ = delivery_trace_fixture()
        baseline, claim, _, _ = self.bounds(receipt, records)
        self.reject(lambda r, rec: self.claim_columns(rec, shift(baseline, -1)))
        self.reject(lambda r, rec: self.claim_columns(rec, shift(claim, 1)))

    def test_claim_columns_at_the_boundaries_are_allowed(self):
        receipt, records, _ = delivery_trace_fixture()
        baseline, claim, _, _ = self.bounds(receipt, records)
        self.accept(lambda r, rec: self.claim_columns(rec, claim))
        started = next(x["started_at"] for x in receipt["observations"]["cases"]
                       if x["case_id"] == "feed-on")
        self.accept(lambda r, rec: self.claim_columns(rec, max(baseline, started)))

    def test_delivered_at_before_the_response_or_after_the_completion_refuses(self):
        receipt, records, _ = delivery_trace_fixture()
        _, _, received, done = self.bounds(receipt, records)
        self.reject(lambda r, rec: set_times(rec, "delivered_at", shift(received, -1), ("completed", "replay")))
        self.reject(lambda r, rec: set_times(rec, "delivered_at", shift(done, 1), ("completed", "replay")))

    def test_delivered_at_at_the_boundaries_is_allowed(self):
        receipt, records, _ = delivery_trace_fixture()
        _, _, received, done = self.bounds(receipt, records)
        self.accept(lambda r, rec: set_times(rec, "delivered_at", done, ("completed", "replay")))
        self.accept(lambda r, rec: set_times(rec, "delivered_at", received, ("completed", "replay")))

    def test_state_times_follow_the_write_effects(self):
        receipt, records, _ = delivery_trace_fixture()
        for ref in (claim_state_ref(receipt), completion_state_ref(receipt)):
            with self.subTest(ref):
                self.reject(lambda r, rec, ref=ref: rec[ref]["data"].update(
                    observed_at=shift(rec[ref]["data"]["observed_at"], 1)))


class States(Storage):
    def refs(self, receipt):
        return {"claimed": claim_state_ref(receipt), "completed": completion_state_ref(receipt)}

    def test_state_presence_must_be_present(self):
        for name, ref in self.refs(delivery_trace_fixture()[0]).items():
            with self.subTest(name):
                self.reject(lambda r, rec, ref=ref: rec[ref]["data"].update(
                    claim={"ref": ref, "presence": "absent"}))

    def test_state_versions_must_advance_one_then_two(self):
        # The fixture uses 1 then 2; the predicate requires increasing versions,
        # not those two literal numbers or a database-owned CAS token.
        refs = self.refs(delivery_trace_fixture()[0])
        for name, versions in (("claimed", ("0", "2", "3", "9")),
                               ("completed", ("0", "1"))):
            for version in versions:
                with self.subTest("%s %s" % (name, version)):
                    self.reject(lambda r, rec, ref=refs[name], version=version:
                                rec[ref]["data"]["claim"].update(version=version))
        for version in ("3", "9"):
            with self.subTest("advancing completion " + version):
                self.accept(lambda r, rec, version=version:
                            rec[refs["completed"]]["data"]["claim"].update(version=version))

        def swap(receipt, records):
            first, second = refs["claimed"], refs["completed"]
            records[first]["data"]["claim"]["version"] = "2"
            records[second]["data"]["claim"]["version"] = "1"
        self.reject(swap)

    def test_stale_value_digests_refuse_without_automatic_repin(self):
        with self.subTest("in-range storage edit without a state repin"):
            def edit(receipt, records):
                baseline = records[delivery_entry(receipt)["before_state_ref"]]["data"]["observed_at"]
                started = next(x["started_at"] for x in receipt["observations"]["cases"]
                               if x["case_id"] == "feed-on")
                lower = max(baseline, started)
                set_times(records, "first_seen_at", lower)
                storage_of(records)["claimed"]["claimed_at"] = lower
            self.reject(edit, "delivery_storage_digest", storage=False)
            self.accept(edit)
        refs = self.refs(delivery_trace_fixture()[0])
        for name, ref in refs.items():
            with self.subTest("state digest " + name):
                self.reject(lambda r, rec, ref=ref: rec[ref]["data"]["claim"].update(
                    value_sha256="0" * 64), storage=False)
        with self.subTest("a swapped digest is not the other row's digest"):
            def swap(receipt, records):
                first, second = refs["claimed"], refs["completed"]
                a, b = records[first]["data"]["claim"], records[second]["data"]["claim"]
                a["value_sha256"], b["value_sha256"] = b["value_sha256"], a["value_sha256"]
            self.reject(swap, storage=False)

    def test_digest_binds_the_projection_not_raw_token_bytes(self):
        receipt, records, _ = delivery_trace_fixture()
        for ref, name in ((claim_state_ref(receipt), "claimed"),
                          (completion_state_ref(receipt), "completed")):
            self.assertEqual(canonical_digest(storage_of(records)[name]),
                             records[ref]["data"]["claim"]["value_sha256"])
        self.assertNotIn("claim_token", storage_of(records)["claimed"])

    def test_state_identity_owner_observer_and_resource_bind_the_delivery_entry(self):
        receipt, _, _ = delivery_trace_fixture()
        changes = {"resource_ref": "feed", "identity_sha256": "0" * 64,
                   "owner_ref": "other-owner", "observer_ref": "other-observer"}
        for ref in self.refs(receipt).values():
            for field, value in changes.items():
                with self.subTest("%s %s" % (ref, field)):
                    self.reject(lambda r, rec, ref=ref, field=field, value=value:
                                rec[ref]["data"].update({field: value}))

    def test_state_claim_ref_must_match_its_record_ref(self):
        for ref in self.refs(delivery_trace_fixture()[0]).values():
            with self.subTest(ref):
                self.reject(lambda r, rec, ref=ref: rec[ref]["data"]["claim"].update(ref="other-state"))

    def test_missing_state_sidecars_refuse(self):
        for ref in self.refs(delivery_trace_fixture()[0]).values():
            with self.subTest(ref):
                self.reject(lambda r, rec, ref=ref: rec.pop(ref))


class Transitions(Storage):
    def test_claim_before_must_be_the_baseline(self):
        receipt, _, _ = delivery_trace_fixture()
        final = delivery_entry(receipt)["after_state_ref"]
        for name, value in (("pending", claim_state_ref(receipt)),
                            ("completed", completion_state_ref(receipt)), ("final", final)):
            with self.subTest(name):
                self.reject(lambda r, rec, value=value: transition(r, rec, "db_insert").update(
                    before_state_ref=value))

    def test_claim_after_must_be_the_pending_state(self):
        receipt, _, _ = delivery_trace_fixture()
        entry = delivery_entry(receipt)
        for name, value in (("baseline", entry["before_state_ref"]), ("final", entry["after_state_ref"]),
                            ("completed", completion_state_ref(receipt))):
            for field in ("intended_after_state_ref", "actual_after_state_ref"):
                with self.subTest("%s %s" % (name, field)):
                    self.reject(lambda r, rec, field=field, value=value:
                                transition(r, rec, "db_insert").update({field: value}))

    def test_completion_before_must_be_pending_and_after_completed(self):
        receipt, _, _ = delivery_trace_fixture()
        entry = delivery_entry(receipt)
        for name, value in (("baseline", entry["before_state_ref"]), ("final", entry["after_state_ref"]),
                            ("completed", completion_state_ref(receipt))):
            with self.subTest("before " + name):
                self.reject(lambda r, rec, value=value: transition(r, rec, "db_cas").update(
                    before_state_ref=value))
        for name, value in (("baseline", entry["before_state_ref"]), ("final", entry["after_state_ref"]),
                            ("pending", claim_state_ref(receipt))):
            for field in ("intended_after_state_ref", "actual_after_state_ref"):
                with self.subTest("%s %s" % (name, field)):
                    self.reject(lambda r, rec, field=field, value=value:
                                transition(r, rec, "db_cas").update({field: value}))

    def test_intended_and_actual_after_must_be_equal(self):
        receipt, _, _ = delivery_trace_fixture()
        entry = delivery_entry(receipt)
        for method in ("db_insert", "db_cas"):
            for field in ("intended_after_state_ref", "actual_after_state_ref"):
                with self.subTest("%s %s" % (method, field)):
                    self.reject(lambda r, rec, method=method, field=field:
                                transition(r, rec, method).update({field: entry["after_state_ref"]}))

    def test_later_cleanup_stays_unqualified_by_storage_slice(self):
        # Full transition/lifecycle composition must check this later write. The
        # storage slice ends at replay and must not be mistaken for that proof.
        receipt, _, _ = delivery_trace_fixture()
        entry = delivery_entry(receipt)
        for name, value in (("baseline", entry["before_state_ref"]),
                            ("pending", claim_state_ref(receipt)),
                            ("final", entry["after_state_ref"])):
            with self.subTest(name):
                receipt, records, now = delivery_trace_fixture()
                transition(receipt, records, "db_delete_owned")["before_state_ref"] = value
                repin_delivery_trace(receipt, records)
                index = evidence_index(records)
                self.assertIsNone(check_delivery_trace_claims(receipt, index, now))
                with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
                    index.require_all_used()

    def test_claim_and_completion_states_must_differ(self):
        def same(receipt, records):
            shared = claim_state_ref(receipt)
            claim = transition(receipt, records, "db_cas")
            claim.update(intended_after_state_ref=shared, actual_after_state_ref=shared)
        self.reject(same)

    def test_replay_may_not_name_an_alternate_row_state(self):
        receipt, _, _ = delivery_trace_fixture()
        for name, ref in (("pending", claim_state_ref(receipt)),
                          ("completed", completion_state_ref(receipt))):
            with self.subTest(name):
                self.reject(lambda r, rec, ref=ref: delivery_effect(r, "db_read").update(
                    db_transition_refs=[ref]))
        with self.subTest("transition ref"):
            self.reject(lambda r, rec: delivery_effect(r, "db_read").update(
                db_transition_refs=[delivery_effect(r, "db_cas")["db_transition_refs"][0]]))


if __name__ == "__main__":
    unittest.main()
