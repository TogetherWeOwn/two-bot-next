"""SYNTHETIC separate recovery ownership-witness claims, NOT actual CAS or authority.

Nothing here is a real compare-and-swap, an authenticated state read, an owner
decision or a cleanup approval. The fixture starts from the recovery installation
fixture and enriches it with the missing state-setup and state-interior records
plus one generic ownership_match witness for the cleanup write. It exercises only
check_recovery_ownership_claims. Success is a partial graph comparison;
runtime_preflight() and the lifecycle phase checker must still refuse. Field
inventories below are literal, not generated from the production schemas, and no
new record kind exists: the witness is the existing ownership_match kind whose
execution_ref names the SEPARATE recovery grant (never the runtime execution),
with the recovery nonce and recovery authority epoch.

State chain (2026-01-01 UTC). The lifecycle fixture's cleanup claim originally
named state-setup as its before state; the real prior state is the latest interior
write, so this fixture corrects cleanup.before_state_ref to state-interior in both
the lifecycle claim and its db_transition record. Setup 00:02:00, interior write
00:15:00, cleanup 00:20:30. The cleanup before state must be observed at exactly
the interior write time. Absent prior: setup inserts v1, interior CAS v2, cleanup
deletes. Present prior: before v1, setup v2, interior v3, cleanup CAS restores the
same value digest as a NEW v4 (the lifecycle after state is moved from v3 to v4 and
every pin is repinned so cleanup can be strictly monotonic).

Some production codes (state reuse, baseline-before-served time, duplicate
cleanup witnesses, a checkpoint other than the named one) are shadowed by
earlier ledger/read checkers or by the witness binding itself, so
they are asserted via the earlier exact code or not at all.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_authority_fixtures import authority_fixture
from announcement_lifecycle_fixtures import (
    CLEANUP, INTERIOR, OTHER_VALUE, SETUP, VALUE, absent, present, state_record)
from announcement_provenance_fixtures import at, envelope, evidence_index
from announcement_runtime_evidence import RECOVERY_RECORDS, TRANSITION_RECORDS, canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_phase_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_recovery import (
    RECOVERY_PLAN_REF, check_recovery_installation_claims, check_recovery_ownership_claims)
from announcement_runtime_schema import Refusal
from test_announcement_runtime_recovery_fences import (
    FENCES, add_second_cleanup, repin_recovery_scope, set_until)
from test_announcement_runtime_recovery_grant import CONSUMPTION, GRANT, NONCE, SCOPE
from test_announcement_runtime_recovery_installations import (
    INSTALLS, approval, approval_ref, install_fixture)
from test_announcement_runtime_recovery_reads import CKPT, CLEAN_CP, FINAL_CP

PRIORS = ("absent", "present")
WITNESS = "match-" + CLEANUP
TRANSITION = "transition-" + CLEANUP
WITNESS_FIELDS = {
    "operation_id", "resource_ref", "identity_sha256", "key_sha256", "owner_ref",
    "execution_ref", "nonce_ref", "authority_epoch", "resource_epoch", "checkpoint_ref",
    "method", "before_state_sha256", "after_state_sha256", "observer_ref", "observed_at",
    "boundary", "outcome"}
INTERIOR_VALUE = "c" * 64
ZERO = "0" * 64


def witness_data(receipt, records, index=0):
    """Build the literal expected cleanup witness from the fixture's own claims."""
    claim = receipt["lifecycle"]["cleanup"][index]
    effect = next(e for e in receipt["observations"]["actual_effects"]
                  if e["operation_id"] == claim["operation_id"])
    resource = next(r for r in receipt["resources"]["entries"] if r["ref"] == claim["resource_ref"])
    grant = records[GRANT]["data"]
    return {
        "operation_id": claim["operation_id"], "resource_ref": resource["ref"],
        "identity_sha256": canonical_digest(resource["identity"]),
        "key_sha256": resource["identity"]["key_sha256"], "owner_ref": resource["owner"],
        "execution_ref": GRANT, "nonce_ref": grant["nonce_ref"],
        "authority_epoch": grant["epoch"], "resource_epoch": grant["resource_epoch"],
        "checkpoint_ref": CLEAN_CP, "method": effect["method"],
        "before_state_sha256": canonical_digest(records[claim["before_state_ref"]]["data"]),
        "after_state_sha256": canonical_digest(records[claim["actual_after_state_ref"]]["data"]),
        "observer_ref": receipt["observations"]["coverage"][0]["observer_ref"],
        "observed_at": effect["observed_at"], "boundary": "external_compare_and_match",
        "outcome": "matched"}


def rewitness(receipt, records, index=0):
    """Recompute the witness from current claims, as a coherent forger would."""
    ref = receipt["lifecycle"]["cleanup"][index]["ownership_match_ref"]
    records[ref] = envelope(ref, "ownership_match", witness_data(receipt, records, index))


def set_claim(receipt, records, index=0, **changes):
    """Change a cleanup ledger claim and its db_transition together."""
    claim = receipt["lifecycle"]["cleanup"][index]
    claim.update(copy.deepcopy(changes))
    records["transition-" + claim["operation_id"]]["data"]["claim"].update(copy.deepcopy(changes))


def edit_state(receipt, records, ref, change, *, witness=True):
    change(records[ref]["data"])
    if witness:
        rewitness(receipt, records)


def set_state_time(receipt, records, ref, clock, *, witness=True):
    edit_state(receipt, records, ref, lambda d: d.update(observed_at=clock), witness=witness)


def set_version(receipt, records, ref, version, *, witness=True):
    edit_state(receipt, records, ref, lambda d: d["claim"].update(version=version), witness=witness)


def make_absent(receipt, records, ref, *, witness=True):
    edit_state(receipt, records, ref, lambda d: d.update(claim=absent(ref)), witness=witness)


def move_interior(receipt, records, clock):
    """Move the interior effect and its transition claim; the state record stays."""
    for field in ("actual_effects", "expected_effects"):
        for effect in receipt["observations"][field]:
            if effect["operation_id"] == INTERIOR:
                effect["observed_at"] = at(clock)
    records["transition-" + INTERIOR]["data"]["claim"]["observed_at"] = at(clock)


def ownership_fixture(prior="absent"):
    """Start from the installation fixture; add states and the recovery witness."""
    receipt, records, now = install_fixture(prior)
    if prior == "absent":
        setup_after = present("state-setup", "1", OTHER_VALUE)
        interior = present("state-interior", "2", INTERIOR_VALUE)
    else:
        setup_after = present("state-setup", "2", OTHER_VALUE)
        interior = present("state-interior", "3", INTERIOR_VALUE)
        # Cleanup must be a strictly newer version than the interior one, while the
        # restored value digest stays equal to the baseline. Move final/after to v4.
        for states in (receipt["resources"]["states"],
                       records["runtime-resource-plan"]["data"]["states"]):
            for state in states:
                if state["ref"] == "state-after":
                    state["version"] = "4"
        for ref in ("state-after", "state-final"):
            records[ref]["data"]["claim"]["version"] = "4"
        repin_recovery_scope(receipt, records)
    records["state-setup"] = state_record(setup_after, "00:02:00")
    records["state-interior"] = state_record(interior, "00:15:00")
    set_claim(receipt, records, before_state_ref="state-interior")
    rewitness(receipt, records)
    return receipt, records, now


def runtime_values(receipt):
    execution = receipt["authority"]["execution"]
    return execution["ref"], execution["nonce_ref"], receipt["authority"]["epoch"]


class RecoveryOwnership(unittest.TestCase):
    def reject(self, mutate, code, *, prior="absent", now=None):
        receipt, records, clock = ownership_fixture(prior)
        mutate(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            check_recovery_ownership_claims(receipt, evidence_index(records), now or clock)

    def accept(self, mutate=lambda r, e: None, *, prior="absent", now=None):
        receipt, records, clock = ownership_fixture(prior)
        mutate(receipt, records)
        self.assertIsNone(check_recovery_ownership_claims(
            receipt, evidence_index(records), now or clock))

    def reject_both(self, mutate, code):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                self.reject(mutate, code, prior=prior)

    # Positive controls and non-promotion.
    def test_coherent_ownership_is_only_partial_comparison(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = ownership_fixture(prior)
                self.assertLessEqual(len(records), 256)
                self.assertIsNone(check_recovery_installation_claims(
                    receipt, evidence_index(records), now))
                self.assertIsNone(check_recovery_ownership_claims(
                    receipt, evidence_index(records), now))
                self.assertFalse(receipt["promotable"])
                with self.assertRaisesRegex(Refusal, "unsupported$"):
                    runtime_preflight()
                # Ordinary checkpoint placeholders still cannot satisfy the full
                # phase comparison, even when this partial recovery layer agrees.
                with self.assertRaises(Refusal):
                    check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_fixture_uses_separate_grant_and_latest_interior_state(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, _ = ownership_fixture(prior)
                runtime_ref, runtime_nonce, runtime_epoch = runtime_values(receipt)
                witness = records[WITNESS]["data"]
                self.assertEqual(set(witness), WITNESS_FIELDS)
                self.assertEqual(witness["execution_ref"], GRANT)
                self.assertEqual(witness["nonce_ref"], NONCE)
                self.assertEqual(witness["authority_epoch"], "11")
                for value, runtime in ((GRANT, runtime_ref), (NONCE, runtime_nonce),
                                       ("11", runtime_epoch)):
                    self.assertNotEqual(value, runtime)
                self.assertEqual(receipt["lifecycle"]["cleanup"][0]["before_state_ref"],
                                 "state-interior")
                self.assertEqual(records[TRANSITION]["data"]["claim"]["before_state_ref"],
                                 "state-interior")
                # Setup and interior witnesses are ordinary-lane data with no record.
                self.assertNotIn("match-" + SETUP, records)
                self.assertNotIn("match-" + INTERIOR, records)

    def test_absent_prior_deletes_and_present_prior_cas_restores_value_with_new_version(self):
        receipt, records, _ = ownership_fixture("absent")
        self.assertEqual(records[WITNESS]["data"]["method"], "db_delete_owned")
        self.assertEqual(records["state-interior"]["data"]["claim"]["presence"], "present")
        self.assertEqual(records["state-final"]["data"]["claim"]["presence"], "absent")
        receipt, records, _ = ownership_fixture("present")
        self.assertEqual(records[WITNESS]["data"]["method"], "db_cas")
        before, interior = (records[r]["data"]["claim"] for r in ("state-before", "state-interior"))
        final = records["state-final"]["data"]["claim"]
        self.assertEqual(final["value_sha256"], before["value_sha256"])
        self.assertEqual(final["value_sha256"], VALUE)
        self.assertEqual(interior["value_sha256"], INTERIOR_VALUE)
        self.assertGreater(int(final["version"]), int(interior["version"]))
        self.assertGreater(int(final["version"]), int(before["version"]))

    def test_ownership_cannot_hide_an_omitted_intermediate_write(self):
        receipt, records, now = ownership_fixture()
        self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))
        # Deliberately omit the interior write while keeping its before-state
        # claim. Compact the projection so ownership, not a sequence gap, refuses.
        receipt["attempt"]["checkpoints"] = authority_fixture()[0]["attempt"]["checkpoints"]
        receipt["lifecycle"]["setup"][0]["authority_ref"] = "checkpoint-1"
        records["transition-" + SETUP]["data"]["claim"]["authority_ref"] = "checkpoint-1"
        receipt["observations"]["actual_effects"] = [effect for effect in
            receipt["observations"]["actual_effects"] if effect["operation_id"] in (SETUP, CLEANUP)]
        for sequence, effect in enumerate(receipt["observations"]["actual_effects"], 1):
            effect["sequence"] = sequence
        with self.assertRaisesRegex(Refusal, "recovery_ownership_chain$"):
            check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_no_new_record_kind_exists_and_witness_schema_is_literal(self):
        self.assertEqual(set(TRANSITION_RECORDS["ownership_match"]), WITNESS_FIELDS)
        for kind in RECOVERY_RECORDS:
            self.assertNotIn("ownership", kind)

    # Mandatory upstream comparisons.
    def test_upstream_installation_and_fence_failures_precede_ownership(self):
        self.reject_both(lambda r, e: e.pop(FENCES), "unresolved_reference")
        self.reject_both(lambda r, e: e.pop(INSTALLS), "unresolved_reference")
        self.reject_both(lambda r, e: e.pop(approval_ref("final")), "unresolved_reference")
        self.reject_both(lambda r, e: approval(e, "cleanup").update(
            checked_at=at("00:20:11")), "recovery_installation_checkpoint")
        self.reject_both(lambda r, e: e[FENCES]["data"].update(recovery_ref="other-recovery"),
                         "recovery_fence_scope")
        self.reject_both(lambda r, e: e.pop(CKPT), "unresolved_reference")

    def test_missing_witness_is_unresolved(self):
        self.reject_both(lambda r, e: e.pop(WITNESS), "unresolved_reference")

    # Witness record shape, kind and attempt.
    def test_witness_is_closed_and_every_field_required(self):
        for field in sorted(WITNESS_FIELDS):
            with self.subTest(field=field):
                self.reject_both(lambda r, e: e[WITNESS]["data"].pop(field), "object_fields")
        for wrong in ({}, None, "extra", "unknown"):
            def mutate(r, e):
                if wrong == "extra":
                    e[WITNESS]["data"]["extra"] = True
                elif wrong == "unknown":
                    e[WITNESS]["data"]["runtime_execution_ref"] = "x"
                else:
                    e[WITNESS]["data"] = wrong
            with self.subTest(wrong=wrong):
                self.reject_both(mutate, "object_fields")

    def test_witness_enums_and_kind_and_attempt(self):
        self.reject_both(lambda r, e: e[WITNESS]["data"].update(boundary="advisory"), "enum")
        self.reject_both(lambda r, e: e[WITNESS]["data"].update(outcome="matched-ish"), "enum")
        self.reject_both(lambda r, e: e[WITNESS]["data"].update(method="db_read"), "enum")
        self.reject_both(lambda r, e: e[WITNESS].update(kind="recovery_bogus"), "evidence_kind")
        self.reject_both(lambda r, e: e[WITNESS]["run"].update(attempt=2), "evidence_attempt")

    def test_other_kinds_cannot_substitute_for_the_witness(self):
        def copy_record(old):
            def mutate(r, e):
                e[WITNESS] = copy.deepcopy(e[old])
                e[WITNESS]["ref"] = WITNESS
            return mutate
        for old in ("state-final", TRANSITION, GRANT, FENCES):
            with self.subTest(old=old):
                self.reject_both(copy_record(old), "reference_kind")

    # Every witness field is compared exactly.
    def test_each_witness_field_mismatch_refuses(self):
        for prior in PRIORS:
            receipt, records, _ = ownership_fixture(prior)
            runtime_ref, runtime_nonce, runtime_epoch = runtime_values(receipt)
            method = records[WITNESS]["data"]["method"]
            other_methods = [m for m in ("db_insert", "db_cas", "db_delete_owned") if m != method]
            wrong = {
                "operation_id": [SETUP, "op-other"], "resource_ref": ["other-resource"],
                "identity_sha256": [ZERO], "key_sha256": [ZERO],
                "owner_ref": ["other-owner"],
                "execution_ref": [runtime_ref, RECOVERY_PLAN_REF, SCOPE, CONSUMPTION, "other-grant"],
                "nonce_ref": [runtime_nonce, CONSUMPTION, "other-nonce"],
                "authority_epoch": [runtime_epoch, "12"], "resource_epoch": ["99"],
                "checkpoint_ref": [FINAL_CP, "other-checkpoint"], "method": other_methods,
                "before_state_sha256": [ZERO], "after_state_sha256": [ZERO],
                "observer_ref": ["other-observer"], "observed_at": [at("00:20:29"), at("00:20:31")],
                "boundary": ["application_only", "unknown"],
                "outcome": ["conflict", "unknown"]}
            self.assertEqual(set(wrong), WITNESS_FIELDS)
            for field, values in wrong.items():
                for value in values:
                    with self.subTest(prior=prior, field=field, value=value):
                        self.reject(lambda r, e: e[WITNESS]["data"].update({field: value}),
                                    "recovery_ownership_match", prior=prior)

    def test_runtime_grant_nonce_and_epoch_substitutions_refuse_together_and_apart(self):
        def substitute(fields):
            def mutate(r, e):
                ref, nonce, epoch = runtime_values(r)
                values = {"execution_ref": ref, "nonce_ref": nonce, "authority_epoch": epoch}
                e[WITNESS]["data"].update({f: values[f] for f in fields})
            return mutate
        for fields in (("execution_ref",), ("nonce_ref",), ("authority_epoch",),
                       ("execution_ref", "nonce_ref"),
                       ("execution_ref", "nonce_ref", "authority_epoch")):
            with self.subTest(fields=fields):
                self.reject_both(substitute(fields), "recovery_ownership_match")

    def test_witness_state_digests_must_match_the_two_resolved_states(self):
        def swap(r, e):
            data = e[WITNESS]["data"]
            data["before_state_sha256"], data["after_state_sha256"] = (
                data["after_state_sha256"], data["before_state_sha256"])
        self.reject_both(swap, "recovery_ownership_match")
        # Digests of the prior setup state are not the interior-state digest even
        # when the value fields are equal, because the state ref is part of the claim.
        def equal_valued_setup(r, e):
            e["state-setup"]["data"]["claim"] = dict(
                copy.deepcopy(e["state-interior"]["data"]["claim"]), ref="state-setup")
            e[WITNESS]["data"]["before_state_sha256"] = canonical_digest(e["state-setup"]["data"])
        self.reject_both(equal_valued_setup, "recovery_ownership_match")

    def test_witness_goes_stale_when_a_bound_state_changes_without_rewitnessing(self):
        self.reject_both(lambda r, e: e["state-interior"]["data"]["claim"].update(
            value_sha256=ZERO), "recovery_ownership_match")

    def test_witness_must_match_operation_checkpoint_method_through_the_ledger(self):
        # A named-checkpoint substitution is caught by the read checker first.
        self.reject_both(lambda r, e: set_claim(r, e, authority_ref=FINAL_CP),
                         "recovery_named_checkpoint")
        self.reject_both(lambda r, e: set_claim(r, e, authority_ref="authority-ref"),
                         "recovery_named_checkpoint")

    # Witness replay.
    def test_ordinary_setup_and_interior_witnesses_cannot_be_replayed(self):
        for ordinary in ("match-" + SETUP, "match-" + INTERIOR):
            def mutate(r, e):
                e[ordinary] = copy.deepcopy(e[WITNESS])
                e[ordinary]["ref"] = ordinary
                set_claim(r, e, ownership_match_ref=ordinary)
            with self.subTest(ordinary=ordinary):
                self.reject_both(mutate, "recovery_ownership_replay")

    def test_witness_ref_must_resolve_after_retargeting(self):
        self.reject_both(lambda r, e: set_claim(r, e, ownership_match_ref="match-missing"),
                         "unresolved_reference")

    # Observer and state identity.
    def test_state_identity_owner_and_ref_are_exact(self):
        for ref in ("state-interior",):
            for label, change in (
                ("claim_ref", lambda d: d["claim"].update(ref="other-state")),
                ("resource", lambda d: d.update(resource_ref="other-resource")),
                ("identity", lambda d: d.update(identity_sha256=ZERO)),
                ("owner", lambda d: d.update(owner_ref="other-owner"))):
                with self.subTest(ref=ref, label=label):
                    self.reject_both(lambda r, e: edit_state(r, e, ref, change),
                                     "recovery_ownership_state")

    def test_final_state_identity_is_checked_by_the_lifecycle_ledger_first(self):
        for label, change in (
            ("claim_ref", lambda d: d["claim"].update(ref="other-state")),
            ("resource", lambda d: d.update(resource_ref="other-resource")),
            ("identity", lambda d: d.update(identity_sha256=ZERO)),
            ("owner", lambda d: d.update(owner_ref="other-owner"))):
            with self.subTest(label=label):
                self.reject_both(lambda r, e: edit_state(r, e, "state-final", change),
                                 "lifecycle_state")

    def test_state_observer_must_be_the_single_coverage_observer(self):
        for ref in ("state-interior", "state-final"):
            with self.subTest(ref=ref):
                self.reject_both(lambda r, e: edit_state(
                    r, e, ref, lambda d: d.update(observer_ref="other-observer")),
                    "recovery_ownership_observer")
        # Changing the coverage observer instead leaves every state unequal to it.
        self.reject_both(lambda r, e: r["observations"]["coverage"][0].update(
            observer_ref="other-observer"), "recovery_ownership_observer")
        # A witness observer alone is a witness mismatch, not a state mismatch.
        self.reject_both(lambda r, e: e[WITNESS]["data"].update(observer_ref="other-observer"),
                         "recovery_ownership_match")

    def test_missing_or_multiple_coverage_observers_refuse(self):
        # Empty coverage refuses in mandatory schema intake before ownership;
        # nonempty duplicate coverage reaches the ownership cardinality check.
        for total, code in ((0, "array_size"), (2, "recovery_ownership_observer")):
            def mutate(r, e):
                original = r["observations"]["coverage"][0]
                r["observations"]["coverage"] = [copy.deepcopy(original) for _ in range(total)]
            with self.subTest(total=total):
                self.reject_both(mutate, code)

    # Prior-state chain.
    def test_cleanup_before_state_must_be_the_latest_prior_write(self):
        for ref in ("state-setup", "state-before", "state-after", "state-final"):
            def mutate(r, e):
                set_claim(r, e, before_state_ref=ref)
                rewitness(r, e)
            with self.subTest(before=ref):
                self.reject_both(mutate, "recovery_ownership_chain")

    def test_chain_substitution_refuses_even_when_state_values_are_equal(self):
        def equal_substitute(r, e):
            e["state-setup"]["data"] = copy.deepcopy(e["state-interior"]["data"])
            e["state-setup"]["data"]["claim"]["ref"] = "state-setup"
            set_claim(r, e, before_state_ref="state-setup")
            rewitness(r, e)
        self.reject_both(equal_substitute, "recovery_ownership_chain")
        # The original uncorrected fixture shape: setup as the before state.
        def uncorrected(r, e):
            set_claim(r, e, before_state_ref="state-setup")
        self.reject_both(uncorrected, "recovery_ownership_chain")

    def test_interior_write_dropping_out_of_the_chain_changes_the_prior_state(self):
        # Without the interior write the prior state would be state-setup, so the
        # corrected before state-interior is no longer the latest prior write.
        def drop_interior(r, e):
            for field in ("actual_effects", "expected_effects"):
                kept = [x for x in r["observations"][field] if x["operation_id"] != INTERIOR]
                for index, item in enumerate(kept, 1):
                    item["sequence"] = index
                r["observations"][field] = kept
        self.reject_both(drop_interior, "recovery_ownership_chain")

    # State and prior-write timing, including a repinned witness.
    def test_before_state_time_must_equal_the_prior_write_time(self):
        for clock in ("00:14:59", "00:15:01", "00:12:00", "00:20:30", "00:20:31"):
            with self.subTest(clock=clock):
                self.reject_both(lambda r, e: set_state_time(
                    r, e, "state-interior", at(clock)), "recovery_ownership_time")
        # Even an unrepinned witness is refused on the time, before the witness.
        self.reject_both(lambda r, e: set_state_time(
            r, e, "state-interior", at("00:14:59"), witness=False), "recovery_ownership_time")
        self.reject_both(lambda r, e: set_state_time(
            r, e, "state-interior", at("00:15:01"), witness=False), "recovery_ownership_time")

    def test_before_state_time_cannot_leave_the_attempt(self):
        self.reject_both(lambda r, e: set_state_time(
            r, e, "state-interior", "2025-12-31T23:59:59Z"), "recovery_ownership_time")

    def test_prior_write_clock_drift_earlier_or_later_refuses(self):
        # The interior state stays at 00:15:00 while the actual prior write drifts.
        for clock in ("00:13:00", "00:14:59", "00:15:01", "00:16:00", "00:19:00"):
            with self.subTest(clock=clock):
                self.reject_both(lambda r, e: (move_interior(r, e, clock), rewitness(r, e)),
                                 "recovery_ownership_time")

    def test_prior_write_and_state_moving_together_still_accepts(self):
        def together(r, e):
            move_interior(r, e, "00:16:00")
            set_state_time(r, e, "state-interior", at("00:16:00"))
        for prior in PRIORS:
            with self.subTest(prior=prior):
                self.accept(together, prior=prior)

    def test_cleanup_after_state_time_is_checked_by_the_ledger_first(self):
        self.reject_both(lambda r, e: set_state_time(
            r, e, "state-final", at("00:20:31")), "lifecycle_final_time")
        self.reject_both(lambda r, e: set_state_time(
            r, e, "state-final", at("00:20:29")), "lifecycle_final_time")

    def test_witness_observed_at_is_the_cleanup_effect_time(self):
        self.reject_both(lambda r, e: e[WITNESS]["data"].update(observed_at=at("00:20:31")),
                         "recovery_ownership_match")

    # Delete and CAS state semantics.
    def test_delete_requires_a_present_before_state(self):
        self.reject(lambda r, e: make_absent(r, e, "state-interior"),
                    "recovery_ownership_delete", prior="absent")

    def test_cas_requires_present_before_state(self):
        self.reject(lambda r, e: make_absent(r, e, "state-interior"),
                    "recovery_ownership_cas", prior="present")

    def test_cas_version_must_increase_strictly(self):
        # Interior v3, cleanup v4: equal and regressed interior versions refuse.
        # "10" is numerically newer than "4" but lexically older; it must refuse.
        for version in ("4", "5", "9", "10"):
            with self.subTest(version=version):
                self.reject(lambda r, e: set_version(r, e, "state-interior", version),
                            "recovery_ownership_cas", prior="present")
        self.accept(lambda r, e: set_version(r, e, "state-interior", "3"), prior="present")

    def test_cas_after_state_cannot_change_value_or_version_without_the_ledger(self):
        # The lifecycle ledger pins the final state to the restored after state.
        def lower_final(r, e):
            e["state-final"]["data"]["claim"]["version"] = "3"
        self.reject(lower_final, "lifecycle_final_state", prior="present")
        def other_value(r, e):
            e["state-final"]["data"]["claim"]["value_sha256"] = OTHER_VALUE
        self.reject(other_value, "lifecycle_final_state", prior="present")

    def test_cleanup_state_refs_cannot_be_reused_via_the_ledger_final_time(self):
        for ref in ("state-before", "state-after"):
            def mutate(r, e):
                set_claim(r, e, intended_after_state_ref=ref, actual_after_state_ref=ref)
            with self.subTest(after=ref):
                self.reject_both(mutate, "lifecycle_final_time")

    # Baseline fallback endpoint: a second present resource with no prior write.
    # Its before state is the resource-graph baseline snapshot, which is validated
    # by the resource/lifecycle graph and need not be observed by the telemetry
    # observer; the cleanup output state and the witness are telemetry-observed.
    def baseline_fixture(self, baseline_observer=None):
        receipt, records, now = ownership_fixture("present")
        add_second_cleanup(receipt, records)
        set_until(records, "cleanup", "00:20:41")
        set_claim(receipt, records, index=1, before_state_ref="state-before-second")
        if baseline_observer is not None:
            records["state-before-second"]["data"]["observer_ref"] = baseline_observer
        rewitness(receipt, records, index=1)
        return receipt, records, now

    def test_second_baseline_resource_cleanup_accepts_with_telemetry_observer(self):
        receipt, records, now = self.baseline_fixture()
        second = receipt["lifecycle"]["cleanup"][1]
        self.assertEqual(second["before_state_ref"], "state-before-second")
        self.assertNotIn("state-setup-second", records)
        self.assertEqual(records["state-before-second"]["data"]["observer_ref"], "observer-ref")
        self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))

    def test_baseline_fallback_observer_may_differ_from_telemetry_observer(self):
        receipt, records, now = self.baseline_fixture("baseline-observer")
        self.assertEqual(records["state-before-second"]["data"]["observer_ref"], "baseline-observer")
        self.assertEqual(records["state-final-second"]["data"]["observer_ref"], "observer-ref")
        self.assertEqual(records["match-cleanup-second"]["data"]["observer_ref"], "observer-ref")
        self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))

    def test_baseline_exemption_still_requires_output_and_witness_telemetry_observer(self):
        def other_after(r, e):
            e["state-final-second"]["data"]["observer_ref"] = "baseline-observer"
        def other_witness(r, e):
            e["match-cleanup-second"]["data"]["observer_ref"] = "baseline-observer"
        for label, mutate, code in (
                ("after", other_after, "recovery_ownership_observer"),
                ("witness", other_witness, "recovery_ownership_match")):
            with self.subTest(label=label):
                receipt, records, now = self.baseline_fixture("baseline-observer")
                mutate(receipt, records)
                with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
                    check_recovery_ownership_claims(receipt, evidence_index(records), now)

    def test_intermediate_before_state_observer_is_still_required_after_a_prior_write(self):
        receipt, records, now = self.baseline_fixture("baseline-observer")
        edit_state(receipt, records, "state-interior",
                   lambda d: d.update(observer_ref="baseline-observer"))
        with self.assertRaisesRegex(
                Refusal, "announcement evidence refused: recovery_ownership_observer$"):
            check_recovery_ownership_claims(receipt, evidence_index(records), now)


if __name__ == "__main__":
    unittest.main()
