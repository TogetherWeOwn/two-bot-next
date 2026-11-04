"""Synthetic offline accounting tests, never authority or staging observations."""

import copy
from datetime import datetime, timedelta, timezone
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import announcement_runtime_policy as policy
from announcement_runtime_schema import Refusal


START = "2026-11-04T12:00:00Z"
NOW = datetime(2026, 11, 4, 12, 0, 30, tzinfo=timezone.utc)
END = "2026-11-04T12:59:59Z"
HASH = "a" * 64
UUIDS = [f"00000000-0000-4000-8000-{i:012d}" for i in range(1, 20)]


def principal(index):
    return {"ref": "principal_" + str(index), "subject_id": UUIDS[index],
            "identity_root_ref": "native_root"}


def document():
    return {"ref": "document", "revision": UUIDS[0], "sha256": HASH}


def authority_fixture():
    decisions = []
    for i, role in enumerate(("security", "qa")):
        decisions.append({
            "ref": role, "role": role, "decision_id": UUIDS[i + 6],
            "stage_id": UUIDS[i + 8], "participant_id": UUIDS[i + 10], "round": 1,
            "principal": principal(i + 3), "scope_sha256": HASH, "epoch": "1",
            "security_decision_id": UUIDS[6], "outcome": "approved",
            "decided_at": f"2026-11-04T11:{i + 50:02d}:00Z",
        })
    reads = [{"ref": consumer + "_read", "consumer": consumer,
              "source_ref": "native_authority", "scope_sha256": HASH, "epoch": "1",
              "resource_epoch": "1", "history_sha256": HASH, "read_at": START,
              "effective_decision_refs": ["security", "qa"]}
             for consumer in ("controller", "verifier")]
    authority = {
        "source_ref": "native_authority", "policy": document(),
        "implementation": {"ref": "implementation", "revision": "b" * 40, "sha256": HASH},
        "scope_sha256": HASH, "resource_plan": document(), "epoch": "1",
        "installation_approval_refs": ["installation"], "executor": principal(0),
        "author": principal(1), "caller": principal(2), "decisions": decisions,
        "reads": reads,
        "execution": {"ref": "execution", "run": {"run_id": "1", "attempt": 1,
                       "control_sha": "b" * 40}, "nonce_ref": "execution_nonce",
                      "consumption_ref": "consumption", "not_before": START,
                      "expires_at": END, "budgets_sha256": HASH},
    }
    checkpoint = {
        "ref": "setup", "phase": "setup", "controller_read_ref": "controller_read",
        "verifier_read_ref": "verifier_read", "checked_at": START,
        "authority_epoch": "1", "resource_epoch": "1", "mutation_fence_ref": "fence",
    }
    return authority, checkpoint


def ledger_fixture():
    resource = {
        "ref": "feed_channel", "kind": "channel",
        "identity": {"kind": "discord", "application_id": "1", "guild_id": "2",
                     "object_id": "3", "object_type": "channel",
                     "permissions_sha256": HASH, "membership_sha256": HASH},
        "owner": "attempt_owner", "allowed_operations": ["feed_post"],
        "expected_count": 1, "maximum_count": 1, "not_before": START, "expires_at": END,
        "before_state_ref": "before", "after_state_ref": "after",
    }
    effect = {
        "operation_id": "post_one", "sequence": 1, "resource_ref": "feed_channel",
        "method": "feed_post", "payload_sha256": HASH, "nonce_ref": ["item_nonce"],
        "attempted_count": 1, "accepted_count": 1, "platform_id_refs": ["4"],
        "db_transition_refs": [], "observed_at": START, "served_ref": "served",
        "outcome": "accepted",
    }
    coverage = {
        "ref": "coverage", "observer_ref": "independent_observer",
        "run": {"run_id": "1", "attempt": 1, "control_sha": "b" * 40},
        "started_at": START, "ended_at": "2026-11-04T12:00:30Z",
        "first_sequence": 1, "last_sequence": 1, "enforcement_epoch": "1",
        "source_inventory_sha256": HASH, "wire_receipt_refs": ["wire_one"],
        "platform_receipt_refs": ["readback"], "transaction_receipt_refs": [],
        "result": "complete",
    }
    return [effect], copy.deepcopy([effect]), coverage, [resource]


class AuthorityPredicates(unittest.TestCase):
    def test_consistent_claims_are_only_offline_consistency(self):
        policy.check_authority_claims(*authority_fixture(), NOW)
        with self.assertRaisesRegex(Refusal, "unsupported"):
            policy.runtime_preflight()

    def test_unusable_clock_is_not_an_authority_fallback(self):
        for bad in (None, "2026-11-04T12:00:30Z", NOW.replace(tzinfo=None)):
            with self.subTest(clock=bad), self.assertRaisesRegex(Refusal, "clock"):
                policy.check_lease(START, END, bad)

    def test_lease_bounds_and_expiry(self):
        policy.check_lease(START, "2026-11-04T13:00:00Z", NOW)
        for end, now in (("2026-11-04T13:00:01Z", NOW), (START, NOW),
                         (END, NOW - timedelta(hours=1)),
                         (END, datetime(2026, 11, 4, 12, 59, 59, tzinfo=timezone.utc))):
            with self.subTest(end=end, now=now), self.assertRaises(Refusal):
                policy.check_lease(START, end, now)

    def test_inclusive_sixty_second_read_limit_not_sixty_one(self):
        authority, checkpoint = authority_fixture()
        policy.check_authority_claims(authority, checkpoint, NOW + timedelta(seconds=30))
        with self.assertRaises(Refusal):
            policy.check_authority_claims(authority, checkpoint, NOW + timedelta(seconds=31))

    def test_order_scope_epoch_and_independence_refusals(self):
        mutations = [
            lambda a, c: a["decisions"].reverse(),
            lambda a, c: a["decisions"][1].update(security_decision_id=UUIDS[17]),
            lambda a, c: a["decisions"][1].update(decided_at=a["decisions"][0]["decided_at"]),
            lambda a, c: a["decisions"][1].update(scope_sha256="c" * 64),
            lambda a, c: a["decisions"][1].update(epoch="2"),
            lambda a, c: a["decisions"][1].update(stage_id=a["decisions"][0]["stage_id"]),
            lambda a, c: a["decisions"][1].update(principal=copy.deepcopy(a["executor"])),
            lambda a, c: a["decisions"][1].update(principal=copy.deepcopy(a["author"])),
            lambda a, c: a["decisions"][1].update(principal=copy.deepcopy(a["caller"])),
            lambda a, c: a["decisions"][1].update(principal=a["decisions"][0]["principal"]),
            lambda a, c: a.update(caller=copy.deepcopy(a["executor"])),
            lambda a, c: c.update(resource_epoch="2"),
            lambda a, c: c.update(authority_epoch="2"),
            lambda a, c: c.update(checked_at="2026-11-04T12:01:00Z"),
            lambda a, c: c.update(verifier_read_ref="controller_read"),
            lambda a, c: a["reads"][1].update(history_sha256="c" * 64),
            lambda a, c: a["reads"][1].update(effective_decision_refs=["qa", "security"]),
            lambda a, c: a["reads"][1].update(source_ref="caller_selected"),
            lambda a, c: a["reads"][1].update(read_at="2026-11-04T12:00:31Z"),
            lambda a, c: a["reads"][1].update(ref="controller_read"),
        ]
        for index, mutate in enumerate(mutations):
            with self.subTest(index=index):
                authority, checkpoint = authority_fixture()
                mutate(authority, checkpoint)
                with self.assertRaises(Refusal):
                    policy.check_authority_claims(authority, checkpoint, NOW)

    def test_effective_decision_refs_cannot_alias_two_distinct_decisions(self):
        authority, checkpoint = authority_fixture()
        authority["decisions"][1]["ref"] = "security"
        for read in authority["reads"]:
            read["effective_decision_refs"] = ["security", "security"]
        with self.assertRaisesRegex(Refusal, "decision_identity"):
            policy.check_authority_claims(authority, checkpoint, NOW)

    def test_gap_ledger_is_not_an_enablement_input(self):
        self.assertGreaterEqual(len(policy.CAPABILITY_GAPS), 12)
        self.assertTrue(all(x.owner for x in policy.CAPABILITY_GAPS))
        for _gap in policy.CAPABILITY_GAPS:
            with self.assertRaisesRegex(Refusal, "unsupported"):
                policy.runtime_preflight()


class EffectAccounting(unittest.TestCase):
    def check(self, fixture):
        policy.check_effect_reconciliation(*fixture, started_at=START,
                                          ended_at="2026-11-04T12:00:30Z", now=NOW)

    def test_exact_multiset(self):
        self.check(ledger_fixture())

    def test_missing_duplicate_wrong_unknown_and_blocked_attempts_refuse(self):
        mutations = [
            lambda e, a, c, r: a.clear(),
            lambda e, a, c, r: a.append(copy.deepcopy(a[0])),
            lambda e, a, c, r: a[0].update(resource_ref="another_channel"),
            lambda e, a, c, r: a[0].update(payload_sha256="c" * 64),
            lambda e, a, c, r: a[0].update(nonce_ref=["another_nonce"]),
            lambda e, a, c, r: a[0].update(platform_id_refs=["5"]),
            lambda e, a, c, r: a[0].update(db_transition_refs=["extra_write"]),
            lambda e, a, c, r: a[0].update(served_ref="another_served"),
            lambda e, a, c, r: a[0].update(sequence=2),
            lambda e, a, c, r: a[0].update(attempted_count=True),
            lambda e, a, c, r: a[0].update(accepted_count=0, outcome="unknown"),
            lambda e, a, c, r: a[0].update(accepted_count=0, outcome="blocked"),
            lambda e, a, c, r: c.update(last_sequence=2),
            lambda e, a, c, r: c.update(result="gap"),
            lambda e, a, c, r: c.update(result="sampled"),
            lambda e, a, c, r: c.update(started_at="2026-11-04T12:00:01Z"),
            lambda e, a, c, r: c.update(ended_at="2026-11-04T12:00:29Z"),
            lambda e, a, c, r: c.update(wire_receipt_refs=[]),
            lambda e, a, c, r: c.update(platform_receipt_refs=[]),
            lambda e, a, c, r: (a[0].update(platform_id_refs=[]),
                                 e[0].update(platform_id_refs=[])),
            lambda e, a, c, r: r[0].update(expected_count=2),
            lambda e, a, c, r: r[0].update(maximum_count=0),
            lambda e, a, c, r: r[0].update(allowed_operations=["db_insert"]),
            lambda e, a, c, r: a[0].update(observed_at="2026-11-04T12:00:31Z"),
            lambda e, a, c, r: r[0].update(expires_at=START),
            lambda e, a, c, r: r.append(copy.deepcopy(r[0])),
        ]
        for index, mutate in enumerate(mutations):
            with self.subTest(index=index):
                fixture = ledger_fixture()
                mutate(*fixture)
                with self.assertRaises(Refusal):
                    self.check(fixture)

    def test_empty_ledger_requires_complete_coverage_not_missing_telemetry(self):
        expected, actual, coverage, resources = ledger_fixture()
        expected.clear()
        actual.clear()
        coverage["last_sequence"] = 0
        resources[0]["expected_count"] = 0
        self.check((expected, actual, coverage, resources))
        coverage["result"] = "unknown"
        with self.assertRaisesRegex(Refusal, "telemetry"):
            self.check((expected, actual, coverage, resources))

    def test_database_effect_requires_transaction_and_transition_receipts(self):
        expected, actual, coverage, resources = ledger_fixture()
        resources[0]["kind"] = "rsvp"
        resources[0]["identity"] = {
            "kind": "database", "account_ref": "test_account", "project_ref": "test_project",
            "endpoint_identity_sha256": HASH, "database_ref": "test_database",
            "schema_ref": "test_schema", "table": "event_rsvps", "key_sha256": HASH,
            "role_ref": "test_role", "grants_sha256": HASH, "trigger_inventory_sha256": HASH,
        }
        resources[0]["allowed_operations"] = ["db_insert"]
        for effect in (expected[0], actual[0]):
            effect.update(method="db_insert", platform_id_refs=[], db_transition_refs=["txn_one"])
        coverage.update(wire_receipt_refs=[], platform_receipt_refs=[],
                        transaction_receipt_refs=["txn_receipt"])
        self.check((expected, actual, coverage, resources))
        coverage["transaction_receipt_refs"] = []
        with self.assertRaisesRegex(Refusal, "missing_transaction_receipts"):
            self.check((expected, actual, coverage, resources))
        coverage["transaction_receipt_refs"] = ["txn_receipt"]
        for effect in (expected[0], actual[0]):
            effect["db_transition_refs"] = []
        with self.assertRaisesRegex(Refusal, "missing_transition_receipts"):
            self.check((expected, actual, coverage, resources))
        for effect in (expected[0], actual[0]):
            effect["method"] = "db_read"
        resources[0]["allowed_operations"] = ["db_read"]
        self.check((expected, actual, coverage, resources))

    def test_effect_clock_must_be_trusted_utc_input(self):
        with self.assertRaisesRegex(Refusal, "clock"):
            policy.check_effect_reconciliation(*ledger_fixture(), started_at=START,
                                              ended_at=START, now=NOW.replace(tzinfo=None))

    def test_no_retry_budget_inferred_from_failed_send(self):
        fixture = ledger_fixture()
        fixture[1][0]["outcome"] = "failed"
        fixture[1][0]["accepted_count"] = 0
        with self.assertRaisesRegex(Refusal, "uncertain_effect"):
            self.check(fixture)


class LifecycleModel(unittest.TestCase):
    def test_entire_window_is_required(self):
        model = policy.LifecycleAccounting()
        with self.assertRaises(Refusal):
            model.require_complete()
        for phase in policy.LIFECYCLE_PHASES:
            model.observe(phase)
        model.require_complete()
        with self.assertRaises(Refusal):
            model.observe("snapshot")

    def test_no_skip_or_repeat_at_any_phase(self):
        for index, phase in enumerate(policy.LIFECYCLE_PHASES):
            model = policy.LifecycleAccounting()
            for previous in policy.LIFECYCLE_PHASES[:index]:
                model.observe(previous)
            with self.subTest(phase=phase), self.assertRaises(Refusal):
                model.observe(policy.LIFECYCLE_PHASES[(index + 1) % len(policy.LIFECYCLE_PHASES)])
            model.observe(phase)
            with self.assertRaises(Refusal):
                model.observe(phase)

    def test_abort_never_recovers_qualification(self):
        for position in range(len(policy.LIFECYCLE_PHASES) + 1):
            model = policy.LifecycleAccounting()
            for phase in policy.LIFECYCLE_PHASES[:position]:
                model.observe(phase)
            model.abort()
            with self.assertRaises(Refusal):
                model.require_complete()
            with self.assertRaises(Refusal):
                model.observe("restoration")


if __name__ == "__main__":
    unittest.main()
