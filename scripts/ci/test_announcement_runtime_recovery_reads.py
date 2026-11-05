"""SYNTHETIC partial recovery read/history claims, NOT approval or authenticated reads.

Nothing here is a real security/QA decision, a live controller or verifier read, a
complete authority history, an installation revalidation or a mutation fence. The
fixture writes literal claims for the closed recovery_read, recovery_history and
recovery_checkpoints records and exercises only check_recovery_read_claims. Success
here is a graph comparison; runtime_preflight() and the lifecycle phase checker
must still refuse. The field inventories below are literal, not generated from
the production schemas.

Timeline (2026-01-01 UTC): grant lease 00:20:00-00:30:00, nonce use 00:20:00,
cleanup checkpoint 00:20:10 for op-cleanup at 00:20:30, restored 00:21:00,
resource final state, attempt end and now at 00:22:00. Decisions were taken at
2025-12-31 23:41:00 and 23:42:00.
"""

import copy
from datetime import datetime, timezone
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_authority_fixtures import authority_fixture
from announcement_lifecycle_fixtures import CLEANUP, SETUP
from announcement_provenance_fixtures import at, envelope, evidence_index, run
from announcement_runtime_evidence import RECOVERY_RECORDS, canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_phase_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_recovery import (
    RECOVERY_CHECKPOINTS_REF, check_recovery_grant_claims, check_recovery_read_claims)
from announcement_runtime_schema import Refusal
from test_announcement_runtime_recovery_grant import GRANT, grant_fixture

CKPT = "runtime-recovery-checkpoints"
CLEAN_CP, FINAL_CP = "recovery-checkpoint-cleanup", "recovery-checkpoint-final"
ROLES = ("controller", "verifier")
PHASES = ("cleanup", "final")
HISTORY = {"cleanup": "recovery-history-cleanup", "final": "recovery-history-final"}
CLEAN_TIME, FINAL_TIME = "00:20:10", "00:22:00"
NOW = datetime(2026, 1, 1, 0, 22, 0, tzinfo=timezone.utc)
STARTED = "2025-12-31T23:00:00Z"
DECIDED = ("2025-12-31T23:41:00Z", "2025-12-31T23:42:00Z")


def read_ref(phase, role):
    return "recovery-read-%s-%s" % (phase, role)


def event(sequence, observed="2025-12-31T23:40:00Z", kind="push", invalidated=("old-decision",),
          scope="0" * 64):
    return {"sequence": sequence, "kind": kind, "observed_at": observed,
            "scope_sha256": scope, "invalidated_decision_refs": list(invalidated)}


def build_history(grant, clock, events):
    return {"recovery_ref": GRANT, "source_ref": "authority-source",
            "scope_sha256": grant["scope_sha256"], "epoch": "11", "resource_epoch": "9",
            "generation": "12", "effective_decision_refs": ["recovery-security", "recovery-qa"],
            "started_at": STARTED, "ended_at": at(clock), "first_sequence": 1,
            "last_sequence": len(events), "coverage": "complete", "events": copy.deepcopy(events)}


def add_phase(records, phase, clock, events=()):
    grant = records[GRANT]["data"]
    history = build_history(grant, clock, list(events))
    records[HISTORY[phase]] = envelope(HISTORY[phase], "recovery_history", history)
    for role in ROLES:
        claim = {"ref": read_ref(phase, role), "consumer": role, "source_ref": "authority-source",
                 "scope_sha256": grant["scope_sha256"], "epoch": "11", "resource_epoch": "9",
                 "history_sha256": canonical_digest(history), "read_at": at(clock),
                 "effective_decision_refs": ["recovery-security", "recovery-qa"]}
        records[claim["ref"]] = envelope(claim["ref"], "recovery_read", {
            "recovery_ref": GRANT, "claim": claim, "history_ref": HISTORY[phase],
            "consumer_ref": role + "-consumer"})


def repin_reads(records, phase):
    digest = canonical_digest(records[HISTORY[phase]]["data"])
    for role in ROLES:
        records[read_ref(phase, role)]["data"]["claim"]["history_sha256"] = digest


def edit_history(records, phase, change, *, repin=True):
    change(records[HISTORY[phase]]["data"])
    if repin:
        repin_reads(records, phase)


def set_events(records, phase, events):
    def change(history):
        history["events"] = copy.deepcopy(list(events))
        history["last_sequence"] = len(events)
    edit_history(records, phase, change)


def set_read_time(records, phase, clock):
    edit_history(records, phase, lambda h: h.update(ended_at=at(clock)))
    for role in ROLES:
        records[read_ref(phase, role)]["data"]["claim"]["read_at"] = at(clock)


def edit_reads(records, change, phases=PHASES):
    for phase in phases:
        for role in ROLES:
            change(records[read_ref(phase, role)]["data"])


def retime_cleanup(receipt, records, clock):
    """Move the single cleanup operation and its sidecars together."""
    receipt["lifecycle"]["cleanup"][0]["observed_at"] = at(clock)
    for effect in (receipt["observations"]["actual_effects"] +
                   receipt["observations"]["expected_effects"]):
        if effect["operation_id"] == CLEANUP:
            effect["observed_at"] = at(clock)
    records["transition-" + CLEANUP]["data"]["claim"]["observed_at"] = at(clock)
    records["state-final"]["data"]["observed_at"] = at(clock)


def set_cleanup_authority(receipt, records, ref):
    receipt["lifecycle"]["cleanup"][0]["authority_ref"] = ref
    records["transition-" + CLEANUP]["data"]["claim"]["authority_ref"] = ref


def read_fixture(prior="absent"):
    """Start from the separate-grant fixture and add literal recovery reads."""
    receipt, records, _ = grant_fixture(prior)
    receipt["attempt"]["ended_at"] = at(FINAL_TIME)
    set_cleanup_authority(receipt, records, CLEAN_CP)
    add_phase(records, "cleanup", CLEAN_TIME)
    add_phase(records, "final", FINAL_TIME)
    records[CKPT] = envelope(CKPT, "recovery_checkpoints", {
        "recovery_ref": GRANT, "run": run(),
        "checkpoints": [
            {"ref": CLEAN_CP, "phase": "cleanup", "operation_id_refs": [CLEANUP],
             "checked_at": at(CLEAN_TIME), "authority_epoch": "11", "resource_epoch": "9",
             "controller_read_ref": read_ref("cleanup", "controller"),
             "verifier_read_ref": read_ref("cleanup", "verifier")},
            {"ref": FINAL_CP, "phase": "final", "operation_id_refs": [],
             "checked_at": at(FINAL_TIME), "authority_epoch": "11", "resource_epoch": "9",
             "controller_read_ref": read_ref("final", "controller"),
             "verifier_read_ref": read_ref("final", "verifier")},
        ]})
    return receipt, records, copy.copy(NOW)


def checkpoints(records):
    return records[CKPT]["data"]["checkpoints"]


class RecoveryReads(unittest.TestCase):
    def reject(self, mutate, code, *, prior="absent", now=None):
        receipt, records, clock = read_fixture(prior)
        mutate(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            check_recovery_read_claims(receipt, evidence_index(records), now or clock)

    def accept(self, mutate=lambda r, e: None, *, prior="absent", now=None):
        receipt, records, clock = read_fixture(prior)
        mutate(receipt, records)
        self.assertIsNone(check_recovery_read_claims(receipt, evidence_index(records), now or clock))

    # Positive controls and non-promotion.
    def test_coherent_partial_reads_are_only_comparison(self):
        for prior in ("absent", "present"):
            with self.subTest(prior=prior):
                receipt, records, now = read_fixture(prior)
                self.assertLessEqual(len(records), 256)
                self.assertEqual(RECOVERY_CHECKPOINTS_REF, CKPT)
                evidence = evidence_index(records)
                self.assertIsNone(check_recovery_grant_claims(receipt, evidence, now))
                self.assertIsNone(check_recovery_read_claims(receipt, evidence_index(records), now))
                self.assertFalse(receipt["promotable"])
                with self.assertRaisesRegex(Refusal, "unsupported$"):
                    runtime_preflight()
                # Existing runtime checkpoint placeholders may refuse before the
                # unsupported_recovery_authority line; either way it must refuse.
                with self.assertRaises(Refusal):
                    check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_current_read_claims_still_cannot_replace_recovery_fence(self):
        receipt, records, now = read_fixture()
        self.assertIsNone(check_recovery_read_claims(receipt, evidence_index(records), now))
        # Isolate the existing mandatory cleanup refusal, not a complete packet.
        receipt["attempt"]["checkpoints"] = authority_fixture()[0]["attempt"]["checkpoints"]
        receipt["lifecycle"]["setup"] = []
        receipt["observations"]["actual_effects"] = [effect for effect in
            receipt["observations"]["actual_effects"] if effect["operation_id"] == CLEANUP]
        with self.assertRaisesRegex(Refusal, "unsupported_recovery_authority$"):
            check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_grant_only_fixture_cannot_satisfy_read_check(self):
        receipt, records, now = grant_fixture()
        with self.assertRaisesRegex(Refusal, "unresolved_reference$"):
            check_recovery_read_claims(receipt, evidence_index(records), now)

    def test_new_kinds_are_registered_with_literal_closed_fields(self):
        self.assertTrue({"recovery_read", "recovery_history", "recovery_checkpoints"}.issubset(
            RECOVERY_RECORDS))
        self.assertEqual(set(RECOVERY_RECORDS["recovery_read"]),
                         {"recovery_ref", "claim", "history_ref", "consumer_ref"})
        self.assertEqual(set(RECOVERY_RECORDS["recovery_history"]), {
            "recovery_ref", "source_ref", "scope_sha256", "epoch", "resource_epoch", "generation",
            "effective_decision_refs", "started_at", "ended_at", "first_sequence",
            "last_sequence", "coverage", "events"})
        self.assertEqual(set(RECOVERY_RECORDS["recovery_checkpoints"]),
                         {"recovery_ref", "run", "checkpoints"})

    # Closed schemas, missing/mixed attempt, unknown and substituted kinds.
    def test_each_new_record_is_closed_and_nonempty(self):
        refs = (CKPT, read_ref("cleanup", "controller"), HISTORY["cleanup"])
        for ref in refs:
            for wrong in ({}, None, "extra", "missing"):
                def mutate(r, e):
                    if wrong == "extra":
                        e[ref]["data"]["extra"] = True
                    elif wrong == "missing":
                        e[ref]["data"].pop(next(iter(e[ref]["data"])))
                    else:
                        e[ref]["data"] = wrong
                with self.subTest(ref=ref, wrong=wrong):
                    self.reject(mutate, "object_fields")

    def test_nested_shapes_are_closed(self):
        self.reject(lambda r, e: checkpoints(e)[0].update(extra=True), "object_fields")
        self.reject(lambda r, e: checkpoints(e)[0].pop("verifier_read_ref"), "object_fields")
        self.reject(lambda r, e: checkpoints(e)[0].update(phase="mutation"), "enum")
        self.reject(lambda r, e: checkpoints(e).pop(), "array_size")
        self.reject(lambda r, e: e[read_ref("cleanup", "controller")]["data"]["claim"].update(
            extra=True), "object_fields")
        self.reject(lambda r, e: e[read_ref("cleanup", "controller")]["data"]["claim"].update(
            consumer="auditor"), "enum")
        self.reject(lambda r, e: e[HISTORY["final"]]["data"]["events"].append(
            dict(event(1), extra=True)) or e[HISTORY["final"]]["data"].update(last_sequence=1),
            "object_fields")
        self.reject(lambda r, e: e[HISTORY["final"]]["data"].update(coverage="partial"), "enum")
        self.reject(lambda r, e: e[HISTORY["final"]]["data"].update(
            events=[event(1, kind="invented")], last_sequence=1), "enum")

    def test_unknown_kind_and_wrong_new_kind_refuse(self):
        for ref in (CKPT, read_ref("cleanup", "verifier"), HISTORY["final"]):
            with self.subTest(ref=ref, case="unknown"):
                self.reject(lambda r, e: e[ref].update(kind="recovery_bogus"), "evidence_kind")
        # Real data of one new kind cannot be relabelled as another new kind.
        self.reject(lambda r, e: e[read_ref("cleanup", "controller")].update(
            kind="recovery_history"), "object_fields")
        self.reject(lambda r, e: e[HISTORY["cleanup"]].update(kind="recovery_read"), "object_fields")
        self.reject(lambda r, e: e[CKPT].update(kind="recovery_history"), "object_fields")

    def test_runtime_kinds_cannot_substitute_for_recovery_kinds(self):
        def copy_runtime(old, ref):
            def mutate(r, e):
                e[ref] = copy.deepcopy(e[old])
                e[ref]["ref"] = ref
            return mutate
        self.reject(copy_runtime("runtime-read-controller-0", read_ref("cleanup", "controller")),
                    "reference_kind")
        self.reject(copy_runtime("runtime-history-0", HISTORY["final"]), "reference_kind")
        self.reject(copy_runtime("authority-source", CKPT), "reference_kind")
        self.reject(lambda r, e: checkpoints(e)[0].update(controller_read_ref="runtime-read-controller-0"),
                    "reference_kind")
        self.reject(lambda r, e: e[read_ref("final", "verifier")]["data"].update(
            history_ref="runtime-history-0"), "reference_kind")

    def test_missing_or_mixed_attempt_records_refuse(self):
        refs = [CKPT, HISTORY["cleanup"], HISTORY["final"]] + [
            read_ref(p, r) for p in PHASES for r in ROLES]
        for ref in refs:
            with self.subTest(ref=ref):
                self.reject(lambda r, e: e.pop(ref), "unresolved_reference")
                self.reject(lambda r, e: e[ref]["run"].update(attempt=2), "evidence_attempt")

    def test_checkpoint_inventory_run_and_recovery_are_exact(self):
        self.reject(lambda r, e: e[CKPT]["data"].update(recovery_ref="other-recovery"),
                    "recovery_checkpoint_scope")
        self.reject(lambda r, e: e[CKPT]["data"]["run"].update(attempt=2),
                    "recovery_checkpoint_scope")
        for index in (0, 1):
            for field, wrong in (("authority_epoch", "7"), ("resource_epoch", "99")):
                with self.subTest(index=index, field=field):
                    self.reject(lambda r, e: checkpoints(e)[index].update({field: wrong}),
                                "recovery_checkpoint_scope")

    # Source/consumer/epoch/generation/scope/decision bindings.
    def test_read_binding_mismatches_refuse(self):
        for phase in PHASES:
            for role, other in (("controller", "verifier"), ("verifier", "controller")):
                ref = read_ref(phase, role)
                cases = (
                    ("recovery_ref", lambda d: d.update(recovery_ref="other-recovery")),
                    ("consumer_ref", lambda d: d.update(consumer_ref=other + "-consumer")),
                    ("consumer", lambda d: d["claim"].update(consumer=other)),
                    ("ref", lambda d: d["claim"].update(ref="other-read")),
                )
                for name, change in cases:
                    with self.subTest(ref=ref, field=name):
                        self.reject(lambda r, e: change(e[ref]["data"]), "recovery_read_binding")

    def test_read_role_swap_refuses(self):
        self.reject(lambda r, e: checkpoints(e)[0].update(
            controller_read_ref=read_ref("cleanup", "verifier"),
            verifier_read_ref=read_ref("cleanup", "controller")), "recovery_read_binding")

    def test_read_source_scope_epochs_and_decisions_are_exact(self):
        wrong = (("source_ref", "other-source"), ("scope_sha256", "f" * 64), ("epoch", "7"),
                 ("resource_epoch", "99"),
                 ("effective_decision_refs", ["recovery-qa", "recovery-security"]),
                 ("effective_decision_refs", ["runtime-security", "runtime-qa"]),
                 ("effective_decision_refs", ["recovery-security"]),
                 ("effective_decision_refs", ["recovery-security", "recovery-qa", "runtime-qa"]))
        for phase in PHASES:
            for role in ROLES:
                for field, value in wrong:
                    with self.subTest(phase=phase, role=role, field=field, value=value):
                        self.reject(lambda r, e: e[read_ref(phase, role)]["data"]["claim"].update(
                            {field: value}), "recovery_read_scope")

    # Freshness.
    def test_read_must_be_inside_lease_and_not_after_checkpoint(self):
        self.reject(lambda r, e: set_read_time(e, "cleanup", "00:19:59"), "stale_recovery_read")
        self.reject(lambda r, e: set_read_time(e, "cleanup", "00:20:11"), "stale_recovery_read")
        self.reject(lambda r, e: set_read_time(e, "final", "00:22:01"), "stale_recovery_read")
        self.accept(lambda r, e: set_read_time(e, "cleanup", "00:20:00"))

    def test_final_boundary_allows_exactly_sixty_seconds(self):
        self.accept(lambda r, e: set_read_time(e, "final", "00:21:00"))
        self.reject(lambda r, e: set_read_time(e, "final", "00:20:59"), "stale_recovery_read")
        self.reject(lambda r, e: set_read_time(e, "final", "00:20:10"), "stale_recovery_read")
        # A later final clock makes an otherwise acceptable final read stale.
        def late(r, e):
            r["attempt"]["ended_at"] = at("00:22:01")
            checkpoints(e)[1]["checked_at"] = at("00:22:01")
            set_read_time(e, "final", "00:21:00")
        receipt, records, _ = read_fixture()
        late(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: stale_recovery_read$"):
            check_recovery_read_claims(receipt, evidence_index(records),
                                       datetime(2026, 1, 1, 0, 22, 1, tzinfo=timezone.utc))

    def test_cleanup_operation_boundary_allows_exactly_sixty_seconds(self):
        def move(clock, read, restored=None):
            def mutate(r, e):
                retime_cleanup(r, e, clock)
                if restored:
                    r["lifecycle"]["served_after"]["observed_at"] = at(restored)
                set_read_time(e, "cleanup", read)
            return mutate
        self.accept(move("00:21:00", "00:20:00"))
        self.accept(move("00:21:00", "00:20:01"))
        # Restoration is moved only so the operation itself remains inside its window.
        self.reject(move("00:21:01", "00:20:00", restored="00:21:30"), "stale_recovery_read")

    def test_expired_grant_is_not_usable_even_with_fresh_claims(self):
        for clock in ("00:30:00", "00:30:01"):
            with self.subTest(clock=clock):
                self.reject(lambda r, e: None, "recovery_expiry",
                            now=datetime(2026, 1, 1, 0, 30, int(clock[-2:]), tzinfo=timezone.utc))

    # History identity and digest pinning.
    def test_history_fields_are_exact_after_repinning(self):
        changes = (("recovery_ref", "other-recovery"), ("source_ref", "other-source"),
                   ("scope_sha256", "f" * 64), ("epoch", "7"), ("resource_epoch", "99"),
                   ("generation", "8"), ("coverage", "gap"), ("coverage", "unknown"),
                   ("effective_decision_refs", ["recovery-qa", "recovery-security"]),
                   ("ended_at", at("00:20:11")))
        for phase in PHASES:
            for field, value in changes:
                with self.subTest(phase=phase, field=field, value=value):
                    self.reject(lambda r, e: edit_history(e, phase, lambda h: h.update({field: value})),
                                "recovery_history")

    def test_history_digest_must_be_repinned(self):
        for phase in PHASES:
            with self.subTest(phase=phase):
                self.reject(lambda r, e: edit_history(
                    e, phase, lambda h: h.update(generation="8"), repin=False), "recovery_history")
                self.reject(lambda r, e: edit_reads(
                    e, lambda d: d["claim"].update(history_sha256="f" * 64), (phase,)),
                    "recovery_history")
        # A read cannot cite the other checkpoint's history even if it is closed.
        self.reject(lambda r, e: e[read_ref("cleanup", "controller")]["data"].update(
            history_ref=HISTORY["final"]), "recovery_history")

    def test_pair_must_agree_on_history_content(self):
        def mutate(r, e):
            alternate = copy.deepcopy(e[HISTORY["cleanup"]])
            alternate["ref"] = "recovery-history-cleanup-verifier"
            alternate["data"]["events"] = [event(1)]
            alternate["data"]["last_sequence"] = 1
            e[alternate["ref"]] = alternate
            data = e[read_ref("cleanup", "verifier")]["data"]
            data["history_ref"] = alternate["ref"]
            data["claim"]["history_sha256"] = canonical_digest(alternate["data"])
        self.reject(mutate, "recovery_read_disagreement")

    def test_each_read_is_used_once(self):
        self.reject(lambda r, e: checkpoints(e)[1].update(
            controller_read_ref=read_ref("cleanup", "controller")), "recovery_read_replay")
        self.reject(lambda r, e: checkpoints(e)[0].update(
            verifier_read_ref=read_ref("cleanup", "controller")), "recovery_read_replay")

    # Gaps, order, invalidation, revocation.
    def test_sequence_gaps_refuse(self):
        def set_raw(events, **fields):
            def mutate(r, e):
                edit_history(e, "final", lambda h: h.update(events=events, **fields))
            return mutate
        self.reject(set_raw([], last_sequence=1), "recovery_history_gap")
        self.reject(set_raw([event(1)], last_sequence=0), "recovery_history_gap")
        self.reject(set_raw([], first_sequence=2, last_sequence=2), "recovery_history_gap")
        self.reject(set_raw([event(2)], last_sequence=1), "recovery_history_gap")
        self.reject(set_raw([event(1), event(1)], last_sequence=2), "recovery_history_gap")
        self.reject(set_raw([event(1), event(3)], last_sequence=3), "recovery_history_gap")

    def test_history_time_and_event_order_refuse(self):
        self.reject(lambda r, e: set_events(e, "final", [
            event(1, "2025-12-31T23:40:00Z"), event(2, "2025-12-31T23:39:00Z")]),
            "recovery_history_time")
        for started in (DECIDED[0], "2025-12-31T23:50:00Z"):
            with self.subTest(started=started):
                self.reject(lambda r, e: edit_history(
                    e, "final", lambda h: h.update(started_at=started)), "recovery_history_time")

    def test_events_must_precede_decisions_and_not_invalidate_them(self):
        self.reject(lambda r, e: set_events(e, "final", [event(1, "2025-12-31T22:59:59Z")]),
                    "invalidated_recovery")
        self.reject(lambda r, e: set_events(e, "final", [event(1, DECIDED[0])]),
                    "invalidated_recovery")
        self.reject(lambda r, e: set_events(e, "final", [event(1, DECIDED[1])]),
                    "invalidated_recovery")
        for kind, invalidated in (("revoke", ["recovery-security"]), ("dismiss", ["recovery-qa"]),
                                  ("policy_change", ["old-decision", "recovery-qa"]),
                                  ("scope_change", ["recovery-security", "recovery-qa"])):
            with self.subTest(kind=kind):
                self.reject(lambda r, e: set_events(e, "cleanup", [
                    event(1, kind=kind, invalidated=invalidated)]), "invalidated_recovery")
        self.reject(lambda r, e: set_events(e, "final", [
            event(1, invalidated=["old-decision", "old-decision"])]), "reference_duplicate")

    def test_prior_unrelated_events_do_not_invalidate_current_decisions(self):
        events = [event(1, "2025-12-31T23:10:00Z", "scope_change", ["older-decision"], "1" * 64),
                  event(2, "2025-12-31T23:40:59Z", "revoke", ["older-decision", "old-decision"])]
        def both(r, e):
            set_events(e, "cleanup", events)
            set_events(e, "final", events)
        for prior in ("absent", "present"):
            with self.subTest(prior=prior):
                self.accept(both, prior=prior)

    # Append-only history across checkpoints.
    def test_identical_history_prefix_is_accepted(self):
        def both(r, e):
            set_events(e, "cleanup", [event(1)])
            set_events(e, "final", [event(1)])
        self.accept(both)

    def test_history_must_be_append_only(self):
        def with_prefix(final_events=None, final_edit=None):
            def mutate(r, e):
                set_events(e, "cleanup", [event(1)])
                set_events(e, "final", [event(1)] if final_events is None else final_events)
                if final_edit:
                    edit_history(e, "final", final_edit)
            return mutate
        self.reject(with_prefix([]), "recovery_history_rewrite")
        self.reject(with_prefix([event(1, "2025-12-31T23:39:30Z")]), "recovery_history_rewrite")
        self.reject(with_prefix([event(1, kind="scope_change")]), "recovery_history_rewrite")
        self.reject(with_prefix(final_edit=lambda h: h.update(started_at="2025-12-31T22:59:00Z")),
                    "recovery_history_rewrite")

    def test_new_events_cannot_be_backfilled_before_the_prior_read(self):
        self.reject(lambda r, e: set_events(e, "final", [event(1)]), "recovery_history_backfill")
        def appended(r, e):
            set_events(e, "cleanup", [event(1)])
            set_events(e, "final", [event(1), event(2, "2025-12-31T23:40:30Z")])
        self.reject(appended, "recovery_history_backfill")

    # Named operation coverage and checkpoint inventory.
    def test_cleanup_operations_must_be_named_exactly_once(self):
        for refs, code in (([], "recovery_checkpoint_operations"),
                           (["op-other"], "recovery_checkpoint_operations"),
                           ([SETUP], "recovery_checkpoint_operations"),
                           ([CLEANUP, SETUP], "recovery_checkpoint_operations"),
                           ([CLEANUP, CLEANUP], "reference_duplicate")):
            with self.subTest(refs=refs):
                self.reject(lambda r, e: checkpoints(e)[0].update(operation_id_refs=list(refs)), code)
        self.reject(lambda r, e: checkpoints(e)[1].update(operation_id_refs=[CLEANUP]),
                    "recovery_checkpoint_inventory")

    def test_cleanup_claim_must_name_its_recovery_checkpoint(self):
        for ref in ("authority-ref", FINAL_CP, "other-recovery-checkpoint"):
            with self.subTest(ref=ref):
                self.reject(lambda r, e: set_cleanup_authority(r, e, ref), "recovery_named_checkpoint")

    def test_runtime_checkpoint_cannot_be_aliased_as_recovery_checkpoint(self):
        # The new checkpoint ref must be distinct from every runtime checkpoint ref.
        # Complete runtime checkpoint objects keep the receipt shape valid.
        runtime_checkpoints = authority_fixture()[0]["attempt"]["checkpoints"]
        self.assertEqual(len(runtime_checkpoints), 6)
        self.reject(lambda r, e: set_cleanup_authority(r, e, "checkpoint-0"),
                    "recovery_named_checkpoint")
        for ref in (CLEAN_CP, FINAL_CP):
            def alias(r, e):
                r["attempt"]["checkpoints"] = copy.deepcopy(runtime_checkpoints)
                r["attempt"]["checkpoints"][0]["ref"] = ref
            with self.subTest(ref=ref):
                self.reject(alias, "recovery_checkpoint_separation")
        self.reject(lambda r, e: checkpoints(e)[1].update(ref=CLEAN_CP), "reference_duplicate")

    def test_checkpoint_inventory_shape_is_exact(self):
        self.reject(lambda r, e: e[CKPT]["data"].update(checkpoints=list(reversed(checkpoints(e)))),
                    "recovery_checkpoint_inventory")
        self.reject(lambda r, e: checkpoints(e)[1].update(phase="cleanup"),
                    "recovery_checkpoint_inventory")
        self.reject(lambda r, e: checkpoints(e)[0].update(phase="final"),
                    "recovery_checkpoint_inventory")
        self.reject(lambda r, e: checkpoints(e)[0].update(checked_at=at("00:22:01")),
                    "recovery_checkpoint_inventory")
        self.reject(lambda r, e: checkpoints(e)[1].update(checked_at=at("00:21:59")),
                    "recovery_checkpoint_inventory")

    def test_checkpoint_and_operation_times_are_ordered(self):
        self.reject(lambda r, e: checkpoints(e)[0].update(checked_at=at("00:19:59")),
                    "recovery_checkpoint_time")
        self.reject(lambda r, e: checkpoints(e)[0].update(checked_at=at("00:20:31")),
                    "recovery_checkpoint_time")
        self.reject(lambda r, e: checkpoints(e)[0].update(checked_at=at("00:22:00")),
                    "recovery_checkpoint_time")
        self.accept(lambda r, e: (checkpoints(e)[0].update(checked_at=at("00:20:30")),
                                  set_read_time(e, "cleanup", "00:20:30")))


if __name__ == "__main__":
    unittest.main()
