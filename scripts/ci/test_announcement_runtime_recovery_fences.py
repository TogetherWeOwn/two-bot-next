"""SYNTHETIC separate recovery mutation-fence claims, NOT acquired or enforced fences.

Nothing here is a real boundary fence, an external atomic guard, an authenticated
read, an installation revalidation or a cleanup approval. The fixture adds literal
recovery_fences and recovery_mutation_fence records on top of the recovery read
fixture and exercises only check_recovery_fence_claims. Success is a graph
comparison; runtime_preflight() and the lifecycle phase checker must still refuse.
The field inventories below are literal, not generated from the production schemas.

Timeline (2026-01-01 UTC): grant lease 00:20:00-00:30:00, cleanup checkpoint
00:20:10 for op-cleanup at 00:20:30, final checkpoint and attempt end 00:22:00,
default now 00:22:00. Default fences expire at 00:20:40 (cleanup) and 00:22:30
(final); the cap is checked_at + 60 seconds and the grant expiry (00:30:00).
"""

import copy
from datetime import datetime, timezone
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_authority_fixtures import authority_fixture
from announcement_lifecycle_fixtures import CLEANUP, SETUP
from announcement_provenance_fixtures import at, envelope, evidence_index
from announcement_runtime_evidence import RECOVERY_RECORDS, canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_phase_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_recovery import (
    RECOVERY_PLAN_REF, check_recovery_fence_claims, check_recovery_grant_claims,
    check_recovery_read_claims)
from announcement_runtime_schema import Refusal
from test_announcement_runtime_recovery import repin
from test_announcement_runtime_recovery_grant import CONSUMPTION, GRANT, NONCE, SCOPE, repin_grant
from test_announcement_runtime_recovery_reads import (
    CKPT, CLEAN_CP, CLEAN_TIME, FINAL_CP, FINAL_TIME, HISTORY, read_fixture,
    read_ref, retime_cleanup, set_read_time)

FENCES = "runtime-recovery-fences"
FENCE = {"cleanup": "recovery-fence-cleanup", "final": "recovery-fence-final"}
CHECKPOINT = {"cleanup": CLEAN_CP, "final": FINAL_CP}
CHECKED = {"cleanup": CLEAN_TIME, "final": FINAL_TIME}
UNTIL = {"cleanup": "00:20:40", "final": "00:22:30"}
PHASES = ("cleanup", "final")
FENCE_FIELDS = {"source_ref", "recovery_ref", "checkpoint_ref", "run", "phase",
                "authority_epoch", "resource_epoch", "nonce_ref", "consumption_ref",
                "observed_at", "valid_until", "boundary"}


def at_second(second):
    return datetime(2026, 1, 1, 0, 22, second, tzinfo=timezone.utc)


def fence_fixture(prior="absent", until=None):
    """Start from the recovery read fixture and add literal fence claims."""
    receipt, records, now = read_fixture(prior)
    until = dict(UNTIL, **(until or {}))
    records[FENCES] = envelope(FENCES, "recovery_fences", {
        "recovery_ref": GRANT, "run": copy.deepcopy(records[GRANT]["data"]["run"]),
        "entries": [{"checkpoint_ref": CHECKPOINT[p], "fence_ref": FENCE[p]} for p in PHASES]})
    for phase in PHASES:
        records[FENCE[phase]] = envelope(FENCE[phase], "recovery_mutation_fence", {
            "source_ref": "authority-source", "recovery_ref": GRANT,
            "checkpoint_ref": CHECKPOINT[phase],
            "run": copy.deepcopy(records[GRANT]["data"]["run"]), "phase": phase,
            "authority_epoch": "11", "resource_epoch": "9", "nonce_ref": NONCE,
            "consumption_ref": CONSUMPTION, "observed_at": at(CHECKED[phase]),
            "valid_until": at(until[phase]), "boundary": "external_atomic"})
    return receipt, records, now


def fence_data(records, phase):
    return records[FENCE[phase]]["data"]


def entries(records):
    return records[FENCES]["data"]["entries"]


def set_until(records, phase, clock):
    fence_data(records, phase)["valid_until"] = at(clock)


def repin_recovery_scope(receipt, records):
    repin(receipt, records)
    scope = records[SCOPE]["data"]
    scope["plan_sha256"] = canonical_digest(records[RECOVERY_PLAN_REF]["data"])
    scope["resource_plan"] = copy.deepcopy(receipt["resources"]["plan"])
    repin_grant(records)
    digest = records[GRANT]["data"]["scope_sha256"]
    for record in records.values():
        if record["kind"] == "recovery_history":
            record["data"]["scope_sha256"] = digest
    for record in records.values():
        if record["kind"] == "recovery_read":
            data = record["data"]
            data["claim"]["scope_sha256"] = digest
            data["claim"]["history_sha256"] = canonical_digest(records[data["history_ref"]]["data"])


def add_second_cleanup(receipt, records, *, split_checkpoint=False):
    """Two distinct literal DB keys; still only a partial offline graph."""
    resource = copy.deepcopy(receipt["resources"]["entries"][0])
    resource["ref"] = "db-resource-second"
    resource["identity"]["key_sha256"] = "f" * 64
    for field in ("before_state_ref", "after_state_ref"):
        resource[field] += "-second"
    receipt["resources"]["entries"].append(resource)
    for old in ("state-before", "state-after", "state-final"):
        ref = old + "-second"
        record = copy.deepcopy(records[old]["data"])
        record.update(resource_ref=resource["ref"], identity_sha256=canonical_digest(resource["identity"]))
        record["claim"]["ref"] = ref
        if old == "state-final":
            record["observed_at"] = at("00:20:40")
        records[ref] = envelope(ref, "resource_state", record)
        if old != "state-final":
            receipt["resources"]["states"].append(copy.deepcopy(record["claim"]))
    operation = "op-cleanup-second"
    claim = copy.deepcopy(receipt["lifecycle"]["cleanup"][0])
    claim.update(operation_id=operation, resource_ref=resource["ref"],
        key_sha256=resource["identity"]["key_sha256"], before_state_ref="state-setup-second",
        intended_after_state_ref="state-final-second", actual_after_state_ref="state-final-second",
        ownership_match_ref="match-cleanup-second", observed_at=at("00:20:40"))
    receipt["lifecycle"]["cleanup"].append(claim)
    transition = copy.deepcopy(records["transition-" + CLEANUP]["data"])
    transition.update(claim=copy.deepcopy(claim), transaction_ref="transaction-cleanup-second")
    records["transition-" + operation] = envelope("transition-" + operation, "db_transition", transition)
    for field in ("actual_effects", "expected_effects"):
        effects = receipt["observations"][field]
        effect = copy.deepcopy(next(e for e in effects if e["operation_id"] == CLEANUP))
        effect.update(operation_id=operation, sequence=len(effects) + 1,
            resource_ref=resource["ref"], observed_at=claim["observed_at"],
            db_transition_refs=["transition-" + operation])
        effects.append(effect)
    plan = records[RECOVERY_PLAN_REF]["data"]
    entry = copy.deepcopy(plan["operations"][0])
    entry.update(operation_id=operation, resource_ref=resource["ref"],
        identity_sha256=canonical_digest(resource["identity"]), key_sha256=claim["key_sha256"],
        restore_state_ref=resource["before_state_ref"])
    plan["operations"].append(entry)
    resource_plan = records["runtime-resource-plan"]["data"]
    resource_plan["entries"] = copy.deepcopy(receipt["resources"]["entries"])
    resource_plan["states"] = copy.deepcopy(receipt["resources"]["states"])
    budget = copy.deepcopy(resource_plan["operation_budgets"][0])
    budget.update(ref="cleanup-budget-second", resource_ref=resource["ref"])
    resource_plan["operation_budgets"].append(budget)
    checkpoints = records[CKPT]["data"]["checkpoints"]
    if not split_checkpoint:
        checkpoints[0]["operation_id_refs"].append(operation)
    else:
        checkpoint = copy.deepcopy(checkpoints[0])
        checkpoint.update(ref="recovery-checkpoint-second", checked_at=at("00:20:35"),
                          operation_id_refs=[operation])
        history = copy.deepcopy(records[HISTORY["cleanup"]]["data"])
        history["ended_at"] = checkpoint["checked_at"]
        history_ref = "recovery-history-second"
        records[history_ref] = envelope(history_ref, "recovery_history", history)
        for role in ("controller", "verifier"):
            ref = "recovery-read-second-" + role
            data = copy.deepcopy(records[read_ref("cleanup", role)]["data"])
            data.update(history_ref=history_ref)
            data["claim"].update(ref=ref, read_at=checkpoint["checked_at"])
            records[ref] = envelope(ref, "recovery_read", data)
            checkpoint[role + "_read_ref"] = ref
        checkpoints.insert(1, checkpoint)
        claim["authority_ref"] = checkpoint["ref"]
        records["transition-" + operation]["data"]["claim"] = copy.deepcopy(claim)
        ref = "recovery-fence-second"
        fence = copy.deepcopy(fence_data(records, "cleanup"))
        fence.update(checkpoint_ref=checkpoint["ref"], observed_at=checkpoint["checked_at"],
                     valid_until=at("00:21:05"))
        records[ref] = envelope(ref, "recovery_mutation_fence", fence)
        entries(records).insert(1, {"checkpoint_ref": checkpoint["ref"], "fence_ref": ref})
    repin_recovery_scope(receipt, records)


class RecoveryFences(unittest.TestCase):
    def reject(self, mutate, code, *, prior="absent", now=None, until=None):
        receipt, records, clock = fence_fixture(prior, until)
        mutate(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            check_recovery_fence_claims(receipt, evidence_index(records), now or clock)

    def accept(self, mutate=lambda r, e: None, *, prior="absent", now=None, until=None):
        receipt, records, clock = fence_fixture(prior, until)
        mutate(receipt, records)
        self.assertIsNone(check_recovery_fence_claims(receipt, evidence_index(records),
                                                      now or clock))

    # Positive controls and non-promotion.
    def test_coherent_partial_fences_are_only_comparison(self):
        for prior in ("absent", "present"):
            with self.subTest(prior=prior):
                receipt, records, now = fence_fixture(prior)
                self.assertLessEqual(len(records), 256)
                evidence = evidence_index(records)
                self.assertIsNone(check_recovery_grant_claims(receipt, evidence, now))
                self.assertIsNone(check_recovery_read_claims(receipt, evidence_index(records), now))
                self.assertIsNone(check_recovery_fence_claims(receipt, evidence_index(records), now))
                self.assertFalse(receipt["promotable"])
                with self.assertRaisesRegex(Refusal, "unsupported$"):
                    runtime_preflight()
                # Ordinary checkpoint placeholders still cannot satisfy the full
                # phase comparison, even when this partial recovery layer agrees.
                with self.assertRaises(Refusal):
                    check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_fences_still_cannot_replace_missing_installation_and_ownership(self):
        receipt, records, now = fence_fixture()
        self.assertIsNone(check_recovery_fence_claims(receipt, evidence_index(records), now))
        # Isolate the incomplete recovery graph, not a complete packet.
        # Compact the projected sequence so a gap does not hide the missing layer.
        receipt["attempt"]["checkpoints"] = authority_fixture()[0]["attempt"]["checkpoints"]
        receipt["lifecycle"]["setup"][0]["authority_ref"] = "checkpoint-1"
        records["transition-" + SETUP]["data"]["claim"]["authority_ref"] = "checkpoint-1"
        receipt["observations"]["actual_effects"] = [effect for effect in
            receipt["observations"]["actual_effects"] if effect["operation_id"] in (SETUP, CLEANUP)]
        for sequence, effect in enumerate(receipt["observations"]["actual_effects"], 1):
            effect["sequence"] = sequence
        with self.assertRaisesRegex(Refusal, "unresolved_reference$"):
            check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_read_only_fixture_cannot_satisfy_fence_check(self):
        receipt, records, now = fence_fixture()
        records.pop(FENCES)
        with self.assertRaisesRegex(Refusal, "unresolved_reference$"):
            check_recovery_fence_claims(receipt, evidence_index(records), now)

    def test_read_failure_precedes_fence_comparison(self):
        self.reject(lambda r, e: e.pop(CKPT), "unresolved_reference")
        self.reject(lambda r, e: e[HISTORY["final"]]["data"].update(generation="8"),
                    "recovery_history")
        self.reject(lambda r, e: e[read_ref("final", "verifier")]["data"]["claim"].update(
            epoch="7"), "recovery_read_scope")

    def test_new_kinds_are_registered_with_literal_closed_fields(self):
        self.assertTrue({"recovery_fences", "recovery_mutation_fence"}.issubset(RECOVERY_RECORDS))
        self.assertEqual(set(RECOVERY_RECORDS["recovery_fences"]),
                         {"recovery_ref", "run", "entries"})
        self.assertEqual(set(RECOVERY_RECORDS["recovery_mutation_fence"]), FENCE_FIELDS)

    # Closed schemas, missing/mixed attempt, unknown and substituted kinds.
    def test_each_new_record_is_closed_and_nonempty(self):
        for ref in (FENCES, FENCE["cleanup"], FENCE["final"]):
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

    def test_every_fence_field_is_required(self):
        for phase in PHASES:
            for field in sorted(FENCE_FIELDS):
                with self.subTest(phase=phase, field=field):
                    self.reject(lambda r, e: fence_data(e, phase).pop(field), "object_fields")
        for field in ("recovery_ref", "run", "entries"):
            with self.subTest(root=field):
                self.reject(lambda r, e: e[FENCES]["data"].pop(field), "object_fields")

    def test_nested_shapes_are_closed(self):
        self.reject(lambda r, e: entries(e)[0].update(extra=True), "object_fields")
        self.reject(lambda r, e: entries(e)[0].pop("fence_ref"), "object_fields")
        self.reject(lambda r, e: entries(e)[1].pop("checkpoint_ref"), "object_fields")
        self.reject(lambda r, e: e[FENCES]["data"]["run"].update(extra=True), "object_fields")
        self.reject(lambda r, e: fence_data(e, "cleanup")["run"].update(extra=True), "object_fields")
        self.reject(lambda r, e: fence_data(e, "cleanup")["run"].pop("attempt"), "object_fields")
        self.reject(lambda r, e: fence_data(e, "cleanup").update(phase="mutation"), "enum")
        self.reject(lambda r, e: fence_data(e, "final").update(boundary="advisory"), "enum")
        self.reject(lambda r, e: fence_data(e, "final").update(boundary="external"), "enum")

    def test_unknown_kind_and_wrong_new_kind_refuse(self):
        for ref in (FENCES, FENCE["cleanup"], FENCE["final"]):
            with self.subTest(ref=ref):
                self.reject(lambda r, e: e[ref].update(kind="recovery_bogus"), "evidence_kind")
        self.reject(lambda r, e: e[FENCE["cleanup"]].update(kind="recovery_fences"),
                    "object_fields")
        self.reject(lambda r, e: e[FENCES].update(kind="recovery_mutation_fence"), "object_fields")
        self.reject(lambda r, e: e[FENCE["final"]].update(kind="recovery_read"), "object_fields")

    def test_runtime_kinds_cannot_substitute_for_recovery_fence_kinds(self):
        def copy_runtime(old, ref):
            def mutate(r, e):
                e[ref] = copy.deepcopy(e[old])
                e[ref]["ref"] = ref
            return mutate
        self.reject(copy_runtime("fence-0", FENCE["cleanup"]), "reference_kind")
        self.reject(copy_runtime("fence-1", FENCE["final"]), "reference_kind")
        self.reject(copy_runtime("authority-source", FENCES), "reference_kind")

    def test_missing_or_mixed_attempt_records_refuse(self):
        for ref in (FENCES, FENCE["cleanup"], FENCE["final"]):
            with self.subTest(ref=ref):
                self.reject(lambda r, e: e.pop(ref), "unresolved_reference")
                self.reject(lambda r, e: e[ref]["run"].update(attempt=2), "evidence_attempt")

    # Scope and inventory.
    def test_inventory_recovery_and_run_are_exact(self):
        self.reject(lambda r, e: e[FENCES]["data"].update(recovery_ref="other-recovery"),
                    "recovery_fence_scope")
        # The recovery scope record and the runtime execution are not the grant.
        self.reject(lambda r, e: e[FENCES]["data"].update(recovery_ref=SCOPE),
                    "recovery_fence_scope")
        self.reject(lambda r, e: e[FENCES]["data"].update(
            recovery_ref=r["authority"]["execution"]["ref"]), "recovery_fence_scope")
        self.reject(lambda r, e: e[FENCES]["data"]["run"].update(attempt=2),
                    "recovery_fence_scope")
        self.reject(lambda r, e: e[FENCES]["data"]["run"].update(run_id="999"),
                    "recovery_fence_scope")
        self.reject(lambda r, e: e[FENCES]["data"]["run"].update(control_sha="f" * 40),
                    "recovery_fence_scope")

    def test_inventory_must_match_checkpoints_exactly(self):
        self.reject(lambda r, e: entries(e).reverse(), "recovery_fence_inventory")
        self.reject(lambda r, e: entries(e).pop(), "array_size")
        self.reject(lambda r, e: entries(e).pop(0), "array_size")
        self.reject(lambda r, e: entries(e).append(
            {"checkpoint_ref": "recovery-checkpoint-extra", "fence_ref": "recovery-fence-extra"}),
            "recovery_fence_inventory")
        self.reject(lambda r, e: entries(e).append(copy.deepcopy(entries(e)[0])),
                    "recovery_fence_inventory")
        self.reject(lambda r, e: entries(e)[1].update(checkpoint_ref=CLEAN_CP),
                    "recovery_fence_inventory")
        self.reject(lambda r, e: entries(e)[0].update(checkpoint_ref=FINAL_CP),
                    "recovery_fence_inventory")
        self.reject(lambda r, e: entries(e)[0].update(checkpoint_ref="other-recovery-checkpoint"),
                    "recovery_fence_inventory")

    def test_duplicate_fence_refs_refuse(self):
        self.reject(lambda r, e: entries(e)[1].update(fence_ref=FENCE["cleanup"]),
                    "reference_duplicate")
        self.reject(lambda r, e: entries(e)[0].update(fence_ref=FENCE["final"]),
                    "reference_duplicate")

    def test_swapped_fence_records_refuse_binding(self):
        self.reject(lambda r, e: (entries(e)[0].update(fence_ref=FENCE["final"]),
                                  entries(e)[1].update(fence_ref=FENCE["cleanup"])),
                    "recovery_fence_binding")

    def test_runtime_fence_cannot_be_aliased_as_recovery_fence(self):
        runtime_checkpoints = authority_fixture()[0]["attempt"]["checkpoints"]
        self.assertEqual(len(runtime_checkpoints), 6)
        for index, phase in enumerate(PHASES):
            def point_at_runtime(r, e):
                r["attempt"]["checkpoints"] = copy.deepcopy(runtime_checkpoints)
                entries(e)[index].update(fence_ref=runtime_checkpoints[0]["mutation_fence_ref"])
            def runtime_uses_ours(r, e):
                r["attempt"]["checkpoints"] = copy.deepcopy(runtime_checkpoints)
                r["attempt"]["checkpoints"][0]["mutation_fence_ref"] = FENCE[phase]
            with self.subTest(phase=phase):
                self.reject(point_at_runtime, "recovery_fence_separation")
                self.reject(runtime_uses_ours, "recovery_fence_separation")

    # Every binding field.
    def test_every_fence_binding_field_is_exact(self):
        wrong = (
            ("source_ref", "other-source"), ("recovery_ref", "other-recovery"),
            ("recovery_ref", SCOPE), ("checkpoint_ref", "other-recovery-checkpoint"),
            ("phase", None), ("authority_epoch", "7"), ("resource_epoch", "99"),
            ("nonce_ref", "other-nonce"), ("consumption_ref", "other-use"),
            ("observed_at", None))
        for phase in PHASES:
            other = "final" if phase == "cleanup" else "cleanup"
            for field, value in wrong:
                if field == "phase":
                    value = other
                elif field == "observed_at":
                    value = at("00:20:11" if phase == "cleanup" else "00:22:01")
                with self.subTest(phase=phase, field=field, value=value):
                    self.reject(lambda r, e: fence_data(e, phase).update({field: value}),
                                "recovery_fence_binding")
            with self.subTest(phase=phase, field="checkpoint_ref", value="other phase"):
                self.reject(lambda r, e: fence_data(e, phase).update(
                    checkpoint_ref=CHECKPOINT[other]), "recovery_fence_binding")

    def test_fence_run_is_exact(self):
        for phase in PHASES:
            for field, value in (("attempt", 2), ("run_id", "999"), ("control_sha", "f" * 40)):
                with self.subTest(phase=phase, field=field):
                    self.reject(lambda r, e: fence_data(e, phase)["run"].update({field: value}),
                                "recovery_fence_binding")

    def test_separate_recovery_nonce_and_scope_cannot_be_runtime_values(self):
        runtime_nonce = lambda r: r["authority"]["execution"]["nonce_ref"]
        for phase in PHASES:
            with self.subTest(phase=phase):
                self.reject(lambda r, e: fence_data(e, phase).update(nonce_ref=runtime_nonce(r)),
                            "recovery_fence_binding")
                self.reject(lambda r, e: fence_data(e, phase).update(
                    recovery_ref=r["authority"]["execution"]["ref"]), "recovery_fence_binding")
                self.reject(lambda r, e: fence_data(e, phase).update(
                    source_ref=r["authority"]["execution"]["ref"]), "recovery_fence_binding")
                self.reject(lambda r, e: fence_data(e, phase).update(consumption_ref=NONCE),
                            "recovery_fence_binding")
                self.reject(lambda r, e: fence_data(e, phase).update(nonce_ref=CONSUMPTION),
                            "recovery_fence_binding")

    # Validity window.
    def test_valid_until_must_follow_checked_at(self):
        for phase in PHASES:
            checked = CHECKED[phase]
            earlier = "00:20:09" if phase == "cleanup" else "00:21:59"
            for clock in (checked, earlier):
                with self.subTest(phase=phase, valid_until=clock):
                    self.reject(lambda r, e: set_until(e, phase, clock), "recovery_fence_time")
        self.accept(lambda r, e: set_until(e, "cleanup", "00:20:31"))
        self.accept(lambda r, e: set_until(e, "final", "00:22:01"), now=at_second(0))

    def test_valid_until_is_capped_at_sixty_seconds(self):
        self.reject(lambda r, e: set_until(e, "cleanup", "00:21:11"), "recovery_fence_time")
        self.reject(lambda r, e: set_until(e, "final", "00:23:01"), "recovery_fence_time")
        self.reject(lambda r, e: set_until(e, "cleanup", "00:25:00"), "recovery_fence_time")
        self.accept(lambda r, e: set_until(e, "cleanup", "00:21:10"))
        self.accept(lambda r, e: set_until(e, "final", "00:23:00"))

    def test_valid_until_far_beyond_lease_refuses(self):
        # The grant expires at 00:30:00, past the sixty-second cap, so the cap
        # fires first; the grant-expiry term of min() is not independently reachable.
        for phase in PHASES:
            for clock in ("00:30:00", "00:30:01", "01:00:00"):
                with self.subTest(phase=phase, valid_until=clock):
                    self.reject(lambda r, e: set_until(e, phase, clock), "recovery_fence_time")

    def test_grant_expiry_caps_fence_even_inside_sixty_second_window(self):
        def shorter_grant(r, e, clock):
            # Rebind every affected content pin and history, not only the expiry
            # label, so the final fence's grant bound is independently exercised.
            plan = e[RECOVERY_PLAN_REF]["data"]
            plan["expires_at"] = at("00:22:20")
            e[GRANT]["data"]["expires_at"] = plan["expires_at"]
            repin_recovery_scope(r, e)
            set_until(e, "final", clock)
        self.reject(lambda r, e: shorter_grant(r, e, "00:22:21"), "recovery_fence_time")
        self.accept(lambda r, e: shorter_grant(r, e, "00:22:20"))

    def test_cleanup_operation_must_be_before_fence_expiry(self):
        # The operation is observed at 00:20:30.
        self.reject(lambda r, e: set_until(e, "cleanup", "00:20:30"), "recovery_mutation_fence_time")
        self.reject(lambda r, e: set_until(e, "cleanup", "00:20:20"), "recovery_mutation_fence_time")
        self.reject(lambda r, e: set_until(e, "cleanup", "00:20:11"), "recovery_mutation_fence_time")
        self.accept(lambda r, e: set_until(e, "cleanup", "00:20:31"))

    def test_cleanup_operation_boundary_at_the_sixty_second_cap(self):
        def move(clock, until):
            def mutate(r, e):
                retime_cleanup(r, e, clock)
                set_read_time(e, "cleanup", "00:20:00")
                set_until(e, "cleanup", until)
            return mutate
        self.accept(move("00:21:00", "00:21:10"))
        self.reject(move("00:21:00", "00:21:00"), "recovery_mutation_fence_time")
        self.reject(move("00:21:00", "00:20:59"), "recovery_mutation_fence_time")

    def test_every_operation_in_one_checkpoint_must_precede_expiry(self):
        self.reject(lambda r, e: add_second_cleanup(r, e), "recovery_mutation_fence_time")
        def unexpired(r, e):
            add_second_cleanup(r, e)
            set_until(e, "cleanup", "00:20:41")
        self.accept(unexpired)

    def test_multiple_cleanup_checkpoints_are_each_checked(self):
        self.accept(lambda r, e: add_second_cleanup(r, e, split_checkpoint=True))
        def second_expired(r, e):
            add_second_cleanup(r, e, split_checkpoint=True)
            set_until(e, "cleanup", "00:20:31")
            e["recovery-fence-second"]["data"]["valid_until"] = at("00:20:40")
        self.reject(second_expired, "recovery_mutation_fence_time")

    def test_final_now_must_be_before_fence_expiry(self):
        self.reject(lambda r, e: None, "recovery_mutation_fence_time", now=at_second(30))
        self.reject(lambda r, e: None, "recovery_mutation_fence_time", now=at_second(31))
        self.reject(lambda r, e: None, "recovery_mutation_fence_time", now=at_second(59))
        self.reject(lambda r, e: set_until(e, "final", "00:23:00"), "recovery_mutation_fence_time",
                    now=datetime(2026, 1, 1, 0, 23, 0, tzinfo=timezone.utc))

    def test_unexpired_boundary_positive_controls(self):
        self.accept(now=at_second(0))
        self.accept(now=at_second(29))
        for prior in ("absent", "present"):
            with self.subTest(prior=prior):
                self.accept(prior=prior, now=at_second(29))
        self.accept(lambda r, e: set_until(e, "final", "00:23:00"), now=at_second(59))
        self.accept(lambda r, e: set_until(e, "cleanup", "00:20:31"))

    def test_expired_grant_is_not_usable_even_with_fresh_fences(self):
        for second in (0, 1):
            with self.subTest(second=second):
                self.reject(lambda r, e: None, "recovery_expiry",
                            now=datetime(2026, 1, 1, 0, 30, second, tzinfo=timezone.utc))


if __name__ == "__main__":
    unittest.main()
