"""SYNTHETIC separate recovery installation-revalidation claims, NOT approvals or installs.

Nothing here is a real security/QA decision, an authenticated read, a complete
authority history, an installation epoch fence or a cleanup approval. The fixture
adds a literal recovery_installations inventory on top of the recovery fence
fixture and, for each recovery checkpoint, copies the reusable
installation-approval-0 anchor with fresh separate approval, read and history
records. It exercises only check_recovery_installation_claims. Success is a
partial graph comparison; runtime_preflight() and the lifecycle phase checker
must still refuse. The field inventories below are literal, not generated from
the production schemas.

Timeline (2026-01-01 UTC): the anchor history ends 00:00:10 and holds one
pre-decision push at 2025-12-31 23:40:00; installation decisions were taken at
23:41:00 and 23:42:00. Cleanup checkpoint 00:20:10 for op-cleanup at 00:20:30,
final checkpoint and default now 00:22:00. New installation reads and histories
end at their checkpoint.
"""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_authority_fixtures import authority_fixture
from announcement_lifecycle_fixtures import SETUP, CLEANUP
from announcement_provenance_fixtures import at, envelope, evidence_index, run
from announcement_runtime_authority import installation_scope
from announcement_runtime_evidence import RECOVERY_RECORDS, canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_phase_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_recovery import (
    check_recovery_fence_claims, check_recovery_installation_claims)
from announcement_runtime_schema import Refusal
from test_announcement_runtime_recovery_fences import (
    CHECKED, CHECKPOINT, FENCES, PHASES, add_second_cleanup, at_second, fence_fixture, set_until)
from test_announcement_runtime_recovery_grant import GRANT
from test_announcement_runtime_recovery_reads import (
    CKPT, CLEAN_CP, FINAL_CP, event, read_ref, retime_cleanup)

INSTALLS = "runtime-recovery-installations"
ANCHOR = "installation-approval-0"
ROLES = ("controller", "verifier")
INSTALL_FIELDS = {"source_ref", "mapping_ref", "policy", "implementation", "control", "epoch",
                  "scope_sha256", "security_round", "decisions", "history_ref",
                  "controller_read_ref", "verifier_read_ref", "checked_at"}
OTHER_UUID = "12345678-1111-4111-8111-111111111111"
POLICY = {"ref": "other-policy", "revision": "12345678-0000-4000-8000-000000000000",
          "sha256": "9" * 64}
IMPLEMENTATION = {"ref": "other-implementation", "revision": "9" * 40, "sha256": "9" * 64}
VERSION = {"commit": "9" * 40, "tree": "9" * 40}


def approval_ref(name):
    return "recovery-installation-" + name


def history_ref(name):
    return "recovery-install-history-" + name


def install_read(name, role):
    return "recovery-install-read-%s-%s" % (name, role)


def add_install(records, name, clock):
    """Copy the anchor approval, its history and both reads at a fresh checkpoint."""
    anchor = records[ANCHOR]["data"]
    history = copy.deepcopy(records[anchor["history_ref"]]["data"])
    history["ended_at"] = at(clock)
    records[history_ref(name)] = envelope(history_ref(name), "authority_history", history)
    approval = copy.deepcopy(anchor)
    approval.update(history_ref=history_ref(name), checked_at=at(clock),
                    controller_read_ref=install_read(name, "controller"),
                    verifier_read_ref=install_read(name, "verifier"))
    for role in ROLES:
        ref = install_read(name, role)
        data = copy.deepcopy(records[anchor[role + "_read_ref"]]["data"])
        data["history_ref"] = history_ref(name)
        data["claim"].update(ref=ref, read_at=at(clock), history_sha256=canonical_digest(history))
        records[ref] = envelope(ref, "authority_read", data)
    records[approval_ref(name)] = envelope(approval_ref(name), "installation_approval", approval)


def install_fixture(prior="absent", until=None):
    """Start from the recovery fence fixture and add literal installation claims."""
    receipt, records, now = fence_fixture(prior, until)
    for phase in PHASES:
        add_install(records, phase, CHECKED[phase])
    records[INSTALLS] = envelope(INSTALLS, "recovery_installations", {
        "recovery_ref": GRANT, "run": copy.deepcopy(records[GRANT]["data"]["run"]),
        "entries": [{"checkpoint_ref": CHECKPOINT[p], "installation_ref": approval_ref(p)}
                    for p in PHASES]})
    return receipt, records, now


def inventory(records):
    return records[INSTALLS]["data"]["entries"]


def approval(records, name):
    return records[approval_ref(name)]["data"]


def history(records, name):
    return records[history_ref(name)]["data"]


def claim(records, name, role):
    return records[install_read(name, role)]["data"]["claim"]


def repin_reads(records, name):
    digest = canonical_digest(history(records, name))
    for role in ROLES:
        claim(records, name, role)["history_sha256"] = digest


def edit_history(records, name, change, *, repin=True):
    change(history(records, name))
    if repin:
        repin_reads(records, name)


def set_events(records, name, events):
    def change(data):
        data["events"] = copy.deepcopy(list(events))
        data["last_sequence"] = len(events)
    edit_history(records, name, change)


def set_install_time(records, name, clock):
    edit_history(records, name, lambda h: h.update(ended_at=at(clock)))
    for role in ROLES:
        claim(records, name, role)["read_at"] = at(clock)


def edit_anchor_history(records, change):
    anchor = records[ANCHOR]["data"]
    data = records[anchor["history_ref"]]["data"]
    change(data)
    for role in ROLES:
        records[anchor[role + "_read_ref"]]["data"]["claim"]["history_sha256"] = canonical_digest(data)


def repin_approval(records, name):
    data = approval(records, name)
    data["scope_sha256"] = canonical_digest(installation_scope(data))


def all_new_refs():
    refs = [INSTALLS]
    for phase in PHASES:
        refs += [approval_ref(phase), history_ref(phase)]
        refs += [install_read(phase, role) for role in ROLES]
    return refs


class RecoveryInstallations(unittest.TestCase):
    def reject(self, mutate, code, *, prior="absent", now=None, until=None):
        receipt, records, clock = install_fixture(prior, until)
        mutate(receipt, records)
        with self.assertRaisesRegex(Refusal, "announcement evidence refused: " + code + "$"):
            check_recovery_installation_claims(receipt, evidence_index(records), now or clock)

    def accept(self, mutate=lambda r, e: None, *, prior="absent", now=None, until=None):
        receipt, records, clock = install_fixture(prior, until)
        mutate(receipt, records)
        self.assertIsNone(check_recovery_installation_claims(
            receipt, evidence_index(records), now or clock))

    # Positive controls and non-promotion.
    def test_coherent_partial_installations_are_only_comparison(self):
        for prior in ("absent", "present"):
            with self.subTest(prior=prior):
                receipt, records, now = install_fixture(prior)
                self.assertLessEqual(len(records), 256)
                self.assertIsNone(check_recovery_fence_claims(receipt, evidence_index(records), now))
                self.assertIsNone(check_recovery_installation_claims(
                    receipt, evidence_index(records), now))
                self.assertFalse(receipt["promotable"])
                with self.assertRaisesRegex(Refusal, "unsupported$"):
                    runtime_preflight()
                # Existing runtime checkpoint placeholders may refuse before the
                # unsupported_recovery_authority line; either way it must refuse.
                with self.assertRaises(Refusal):
                    check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_installations_still_cannot_replace_unsupported_recovery_refusal(self):
        receipt, records, now = install_fixture()
        self.assertIsNone(check_recovery_installation_claims(
            receipt, evidence_index(records), now))
        # Isolate the existing mandatory cleanup refusal, not a complete packet.
        receipt["attempt"]["checkpoints"] = authority_fixture()[0]["attempt"]["checkpoints"]
        receipt["lifecycle"]["setup"][0]["authority_ref"] = "checkpoint-1"
        records["transition-" + SETUP]["data"]["claim"]["authority_ref"] = "checkpoint-1"
        receipt["observations"]["actual_effects"] = [effect for effect in
            receipt["observations"]["actual_effects"] if effect["operation_id"] in (SETUP, CLEANUP)]
        with self.assertRaisesRegex(Refusal, "unsupported_recovery_authority$"):
            check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_fence_only_fixture_cannot_satisfy_installation_check(self):
        receipt, records, now = install_fixture()
        records.pop(INSTALLS)
        with self.assertRaisesRegex(Refusal, "unresolved_reference$"):
            check_recovery_installation_claims(receipt, evidence_index(records), now)

    def test_fence_failure_precedes_installation_comparison(self):
        self.reject(lambda r, e: e.pop(FENCES), "unresolved_reference")
        self.reject(lambda r, e: set_until(e, "cleanup", "00:20:30"), "recovery_mutation_fence_time")
        self.reject(lambda r, e: None, "recovery_mutation_fence_time", now=at_second(31))

    def test_new_kind_is_registered_with_literal_closed_fields(self):
        self.assertIn("recovery_installations", RECOVERY_RECORDS)
        schema = RECOVERY_RECORDS["recovery_installations"]
        self.assertEqual(set(schema), {"recovery_ref", "run", "entries"})
        self.assertEqual(set(schema["entries"].item), {"checkpoint_ref", "installation_ref"})
        self.assertEqual(schema["entries"].minimum, 2)

    # Closed schemas, missing/mixed attempt, unknown and substituted kinds.
    def test_each_new_record_is_closed_and_nonempty(self):
        for ref in (INSTALLS, approval_ref("cleanup"), approval_ref("final"),
                    history_ref("cleanup"), install_read("final", "verifier")):
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

    def test_every_inventory_and_approval_field_is_required(self):
        for field in ("recovery_ref", "run", "entries"):
            with self.subTest(root=field):
                self.reject(lambda r, e: e[INSTALLS]["data"].pop(field), "object_fields")
        for phase in PHASES:
            for field in sorted(INSTALL_FIELDS):
                with self.subTest(phase=phase, field=field):
                    self.reject(lambda r, e: approval(e, phase).pop(field), "object_fields")

    def test_nested_shapes_are_closed(self):
        self.reject(lambda r, e: inventory(e)[0].update(extra=True), "object_fields")
        self.reject(lambda r, e: inventory(e)[0].pop("installation_ref"), "object_fields")
        self.reject(lambda r, e: inventory(e)[1].pop("checkpoint_ref"), "object_fields")
        self.reject(lambda r, e: e[INSTALLS]["data"]["run"].update(extra=True), "object_fields")
        self.reject(lambda r, e: e[INSTALLS]["data"]["run"].pop("attempt"), "object_fields")
        self.reject(lambda r, e: approval(e, "cleanup")["decisions"][0].update(extra=True),
                    "object_fields")
        self.reject(lambda r, e: approval(e, "final")["decisions"].pop(), "array_size")
        self.reject(lambda r, e: history(e, "cleanup")["events"][0].update(extra=True),
                    "object_fields")
        self.reject(lambda r, e: claim(e, "final", "controller").update(extra=True), "object_fields")

    def test_unknown_kind_and_wrong_new_kind_refuse(self):
        for ref in all_new_refs():
            with self.subTest(ref=ref):
                self.reject(lambda r, e: e[ref].update(kind="recovery_bogus"), "evidence_kind")
        self.reject(lambda r, e: e[INSTALLS].update(kind="recovery_fences"), "object_fields")
        self.reject(lambda r, e: e[INSTALLS].update(kind="installation_approval"), "object_fields")
        self.reject(lambda r, e: e[approval_ref("cleanup")].update(kind="recovery_installations"),
                    "object_fields")

    def test_existing_kinds_cannot_substitute_for_recovery_installation_records(self):
        def copy_record(old, ref):
            def mutate(r, e):
                e[ref] = copy.deepcopy(e[old])
                e[ref]["ref"] = ref
            return mutate
        self.reject(copy_record(FENCES, INSTALLS), "reference_kind")
        self.reject(copy_record("install-history-0", approval_ref("cleanup")), "reference_kind")
        self.reject(copy_record("install-history-0", install_read("cleanup", "controller")),
                    "reference_kind")
        self.reject(copy_record("install-read-controller-0", history_ref("final")),
                    "reference_kind")
        self.reject(copy_record("install-read-controller-0", approval_ref("final")),
                    "reference_kind")

    def test_missing_or_mixed_attempt_records_refuse(self):
        for ref in all_new_refs():
            with self.subTest(ref=ref):
                self.reject(lambda r, e: e.pop(ref), "unresolved_reference")
                self.reject(lambda r, e: e[ref]["run"].update(attempt=2), "evidence_attempt")

    # Scope and inventory.
    def test_inventory_recovery_and_run_are_exact(self):
        scope = "recovery_installation_scope"
        self.reject(lambda r, e: e[INSTALLS]["data"].update(recovery_ref="other-recovery"), scope)
        # The runtime execution is not the grant, and a plan is not a grant.
        self.reject(lambda r, e: e[INSTALLS]["data"].update(
            recovery_ref=r["authority"]["execution"]["ref"]), scope)
        self.reject(lambda r, e: e[INSTALLS]["data"]["run"].update(attempt=2), scope)
        self.reject(lambda r, e: e[INSTALLS]["data"]["run"].update(run_id="999"), scope)
        self.reject(lambda r, e: e[INSTALLS]["data"]["run"].update(control_sha="f" * 40), scope)

    def test_inventory_must_match_checkpoints_exactly(self):
        code = "recovery_installation_inventory"
        self.reject(lambda r, e: inventory(e).reverse(), code)
        self.reject(lambda r, e: inventory(e).pop(), "array_size")
        self.reject(lambda r, e: inventory(e).pop(0), "array_size")
        self.reject(lambda r, e: inventory(e).append(
            {"checkpoint_ref": "recovery-checkpoint-extra",
             "installation_ref": "recovery-installation-extra"}), code)
        self.reject(lambda r, e: inventory(e).append(copy.deepcopy(inventory(e)[0])), code)
        self.reject(lambda r, e: inventory(e)[1].update(checkpoint_ref=CLEAN_CP), code)
        self.reject(lambda r, e: inventory(e)[0].update(checkpoint_ref=FINAL_CP), code)
        self.reject(lambda r, e: inventory(e)[0].update(checkpoint_ref="other-checkpoint"), code)

    def test_duplicate_and_replayed_installation_refs_refuse(self):
        self.reject(lambda r, e: inventory(e)[1].update(
            installation_ref=approval_ref("cleanup")), "reference_duplicate")
        self.reject(lambda r, e: inventory(e)[0].update(
            installation_ref=approval_ref("final")), "reference_duplicate")
        for index in (0, 1):
            for prior in ("installation-approval-0", "installation-approval-1",
                          "installation-approval-5"):
                with self.subTest(index=index, prior=prior):
                    self.reject(lambda r, e: inventory(e)[index].update(installation_ref=prior),
                                "recovery_installation_replay")

    def test_wrong_kind_installation_ref_refuses(self):
        self.reject(lambda r, e: inventory(e)[0].update(installation_ref=FENCES), "reference_kind")
        self.reject(lambda r, e: inventory(e)[1].update(installation_ref=CKPT), "reference_kind")
        self.reject(lambda r, e: inventory(e)[1].update(installation_ref="missing-approval"),
                    "unresolved_reference")

    def test_swapped_approvals_refuse_checkpoint_binding(self):
        self.reject(lambda r, e: (inventory(e)[0].update(installation_ref=approval_ref("final")),
                                  inventory(e)[1].update(installation_ref=approval_ref("cleanup"))),
                    "recovery_installation_checkpoint")
        for phase in PHASES:
            with self.subTest(phase=phase):
                self.reject(lambda r, e: approval(e, phase).update(
                    checked_at=at("00:20:11" if phase == "cleanup" else "00:22:01")),
                    "recovery_installation_checkpoint")

    # Anchor, installed mapping and static scope.
    def test_anchor_must_match_control_and_precede_the_grant(self):
        self.reject(lambda r, e: r["authority"]["installation_approval_refs"].reverse(),
                    "recovery_installation_anchor")
        self.reject(lambda r, e: e[ANCHOR]["data"].update(checked_at="2025-12-31T23:59:59Z"),
                    "recovery_installation_anchor")
        self.reject(lambda r, e: e[ANCHOR]["data"].update(checked_at=at("00:20:01")),
                    "recovery_installation_anchor")

    def test_installed_mapping_is_exact(self):
        mapping = lambda r: r["control"]["installation"]["mapping_ref"]
        self.reject(lambda r, e: e[mapping(r)]["data"].update(pr="99999"), "installation_mapping")
        self.reject(lambda r, e: e[mapping(r)]["data"].update(head=VERSION), "installation_mapping")
        self.reject(lambda r, e: e[mapping(r)]["data"].update(squash=VERSION), "installation_mapping")
        self.reject(lambda r, e: e[mapping(r)]["data"].update(control=VERSION), "installation_mapping")

    def test_every_static_scope_field_is_exact(self):
        wrong = (("source_ref", "other-source"), ("mapping_ref", "other-mapping"),
                 ("policy", POLICY), ("implementation", IMPLEMENTATION), ("control", VERSION),
                 ("epoch", "4"), ("scope_sha256", "0" * 64))
        for phase in PHASES:
            for field, value in wrong:
                for repin in (False, True):
                    def mutate(r, e):
                        approval(e, phase)[field] = copy.deepcopy(value)
                        if repin:
                            repin_approval(e, phase)
                    with self.subTest(phase=phase, field=field, repin=repin):
                        if field == "scope_sha256" and repin:
                            # Rehashing the unchanged scope restores its original digest.
                            self.accept(mutate)
                        else:
                            self.reject(mutate, "recovery_installation_approval")

    def test_anchor_decisions_stages_and_rounds_are_unchanged(self):
        wrong = (("ref", "other-decision"), ("decision_id", OTHER_UUID),
                 ("stage_id", OTHER_UUID), ("participant_id", OTHER_UUID), ("round", 3),
                 ("epoch", "4"), ("scope_sha256", "0" * 64), ("security_decision_id", OTHER_UUID),
                 ("decided_at", "2025-12-31T23:43:00Z"))
        for phase in PHASES:
            for index in (0, 1):
                for field, value in wrong:
                    with self.subTest(phase=phase, index=index, field=field):
                        self.reject(lambda r, e: approval(e, phase)["decisions"][index].update(
                            {field: value}), "recovery_installation_approval")
            with self.subTest(phase=phase, change="order"):
                self.reject(lambda r, e: approval(e, phase)["decisions"].reverse(),
                            "recovery_installation_approval")
        # The anchor itself drifting also leaves the new approvals unequal to it.
        self.reject(lambda r, e: e[ANCHOR]["data"]["decisions"][0].update(
            decided_at="2025-12-31T23:40:30Z"), "recovery_installation_approval")
        self.reject(lambda r, e: approval(e, "cleanup").update(security_round=3),
                    "security_round_binding")
        self.reject(lambda r, e: approval(e, "final").update(security_round=1),
                    "security_round_binding")

    # Both read roles.
    def test_each_read_role_is_bound_to_its_consumer_and_history(self):
        code = "recovery_installation_read"
        for phase in PHASES:
            other = "final" if phase == "cleanup" else "cleanup"
            for role in ROLES:
                other_role = "verifier" if role == "controller" else "controller"
                ref = install_read(phase, role)
                for label, mutate in (
                    ("ref", lambda r, e: claim(e, phase, role).update(ref="other-read")),
                    ("consumer", lambda r, e: claim(e, phase, role).update(consumer=other_role)),
                    ("consumer_ref", lambda r, e: e[ref]["data"].update(consumer_ref="other-consumer")),
                    ("swapped_consumer_ref", lambda r, e: e[ref]["data"].update(
                        consumer_ref=other_role + "-consumer")),
                    ("record_history", lambda r, e: e[ref]["data"].update(
                        history_ref=history_ref(other))),
                    ("approval_history", lambda r, e: approval(e, phase).update(
                        history_ref=history_ref(other))),
                    ("anchor_history", lambda r, e: approval(e, phase).update(
                        history_ref=r and e[ANCHOR]["data"]["history_ref"])),
                    ("swapped_read", lambda r, e: approval(e, phase).update(
                        {role + "_read_ref": install_read(phase, other_role)}))):
                    with self.subTest(phase=phase, role=role, mutation=label):
                        # The controller read has already been consumed when the
                        # verifier aliases it; replay refuses before role binding.
                        expected = ("recovery_installation_replay" if
                                    label == "swapped_read" and role == "verifier" else code)
                        self.reject(mutate, expected)

    def test_each_read_role_scope_and_digest_are_exact(self):
        wrong = (("source_ref", "other-source", "recovery_installation_read_scope"),
                 ("scope_sha256", "0" * 64, "recovery_installation_read_scope"),
                 ("epoch", "4", "recovery_installation_read_scope"),
                 ("resource_epoch", "99", "recovery_installation_read_scope"),
                 ("effective_decision_refs", ["install-qa", "install-security"],
                  "recovery_installation_read_scope"),
                 ("effective_decision_refs", ["install-security", "recovery-qa"],
                  "recovery_installation_read_scope"),
                 ("history_sha256", "0" * 64, "recovery_installation_history"))
        for phase in PHASES:
            for role in ROLES:
                for field, value, code in wrong:
                    with self.subTest(phase=phase, role=role, field=field):
                        self.reject(lambda r, e: claim(e, phase, role).update({field: value}), code)

    def test_no_prior_runtime_or_recovery_reads_are_reused(self):
        reused = ("runtime-read-controller-0", "runtime-read-verifier-1", "install-read-controller-0",
                  "install-read-verifier-1", "install-read-controller-5",
                  read_ref("cleanup", "controller"), read_ref("final", "verifier"))
        for phase in PHASES:
            for role in ROLES:
                for ref in reused:
                    with self.subTest(phase=phase, role=role, ref=ref):
                        self.reject(lambda r, e: approval(e, phase).update({role + "_read_ref": ref}),
                                    "recovery_installation_replay")
        # One read cannot serve both roles or both checkpoints.
        self.reject(lambda r, e: approval(e, "cleanup").update(
            verifier_read_ref=install_read("cleanup", "controller")), "recovery_installation_replay")
        self.reject(lambda r, e: approval(e, "final").update(
            controller_read_ref=install_read("cleanup", "controller")), "recovery_installation_replay")

    def test_anchor_reads_may_precede_attempt_inside_execution_lease(self):
        # The historical anchor was read under the runtime execution lease (from
        # 00:00:00), which may open before the attempt itself starts.
        def early_anchor(r, e):
            self.assertEqual(r["authority"]["execution"]["not_before"], at("00:00:00"))
            r["attempt"]["started_at"] = at("00:00:05")
            edit_anchor_history(e, lambda h: h.update(ended_at=at("00:00:00")))
            for role in ROLES:
                e[e[ANCHOR]["data"][role + "_read_ref"]]["data"]["claim"]["read_at"] = at("00:00:00")
        for prior in ("absent", "present"):
            with self.subTest(prior=prior):
                self.accept(early_anchor, prior=prior)

    # Freshness.
    def test_read_must_be_inside_lease_and_not_after_checkpoint(self):
        stale = "stale_recovery_installation_read"
        self.reject(lambda r, e: set_install_time(e, "cleanup", "00:19:59"), stale)
        self.reject(lambda r, e: set_install_time(e, "cleanup", "00:20:11"), stale)
        self.reject(lambda r, e: set_install_time(e, "final", "00:22:01"), stale)
        self.accept(lambda r, e: set_install_time(e, "cleanup", "00:20:00"))

    def test_final_now_allows_exactly_sixty_seconds(self):
        self.accept(lambda r, e: set_install_time(e, "final", "00:21:00"))
        self.reject(lambda r, e: set_install_time(e, "final", "00:21:00"),
                    "stale_recovery_installation_read", now=at_second(1))
        self.reject(lambda r, e: set_install_time(e, "final", "00:20:59"),
                    "stale_recovery_installation_read")
        self.reject(lambda r, e: set_install_time(e, "final", "00:20:10"),
                    "stale_recovery_installation_read")

    def test_cleanup_mutation_allows_exactly_sixty_seconds(self):
        def move(clock, read, restored=None):
            def mutate(r, e):
                retime_cleanup(r, e, clock)
                if restored:
                    r["lifecycle"]["served_after"]["observed_at"] = at(restored)
                set_until(e, "cleanup", "00:21:10")
                set_install_time(e, "cleanup", read)
            return mutate
        self.accept(move("00:21:00", "00:20:00"))
        self.accept(move("00:21:00", "00:20:01"))
        self.accept(move("00:21:01", "00:20:01", restored="00:21:30"))
        # Restoration is moved only so the operation itself remains inside its window.
        self.reject(move("00:21:01", "00:20:00", restored="00:21:30"),
                    "stale_recovery_installation_read")

    def test_recovery_reads_are_not_installation_freshness(self):
        # Fresh recovery reads cannot rescue a stale installation read.
        self.reject(lambda r, e: set_install_time(e, "final", "00:20:00"),
                    "stale_recovery_installation_read")

    # History.
    def test_history_identity_and_coverage_are_exact(self):
        code = "recovery_installation_history"
        edits = (("source_ref", "other-source"), ("scope_sha256", "0" * 64), ("epoch", "4"),
                 ("resource_epoch", "99"),
                 ("effective_decision_refs", ["install-qa", "install-security"]),
                 ("coverage", "gap"), ("coverage", "unknown"))
        for phase in PHASES:
            for field, value in edits:
                with self.subTest(phase=phase, field=field, value=value):
                    self.reject(lambda r, e: edit_history(e, phase, lambda h: h.update({field: value})),
                                code)
            with self.subTest(phase=phase, field="ended_at"):
                self.reject(lambda r, e: edit_history(e, phase, lambda h: h.update(
                    ended_at=at("00:00:09"))), code)
            with self.subTest(phase=phase, field="unpinned"):
                self.reject(lambda r, e: edit_history(e, phase, lambda h: h.update(
                    started_at="2025-12-31T22:00:00Z"), repin=False), code)

    def test_incomplete_or_gapped_history_refuses(self):
        code = "recovery_installation_history_gap"
        for phase in PHASES:
            with self.subTest(phase=phase):
                self.reject(lambda r, e: edit_history(e, phase, lambda h: h.update(last_sequence=2)), code)
                self.reject(lambda r, e: edit_history(e, phase, lambda h: h.update(first_sequence=2)), code)
                self.reject(lambda r, e: edit_history(
                    e, phase, lambda h: h["events"][0].update(sequence=2)), code)
                self.reject(lambda r, e: set_events(e, phase, [event(1), event(3)]), code)

    def test_history_must_start_before_decisions_and_be_ordered(self):
        code = "recovery_installation_history_time"
        for phase in PHASES:
            with self.subTest(phase=phase):
                self.reject(lambda r, e: edit_history(e, phase, lambda h: h.update(
                    started_at="2025-12-31T23:41:00Z")), code)
                self.reject(lambda r, e: set_events(e, phase, [
                    event(1, observed="2025-12-31T23:40:10Z"),
                    event(2, observed="2025-12-31T23:40:05Z")]), code)

    def test_invalidating_or_out_of_window_events_refuse(self):
        code = "invalidated_recovery_installation"
        for phase in PHASES:
            for label, events in (
                ("security", [event(1, invalidated=("install-security",))]),
                ("qa", [event(1, invalidated=("old-decision", "install-qa"))]),
                ("at_decision", [event(1, observed="2025-12-31T23:41:00Z")]),
                ("after_decision", [event(1, observed="2025-12-31T23:50:00Z")]),
                ("before_start", [event(1, observed="2025-12-31T22:59:59Z")])):
                with self.subTest(phase=phase, case=label):
                    self.reject(lambda r, e: set_events(e, phase, events), code)
        self.reject(lambda r, e: set_events(e, "final", [
            event(1, invalidated=("old-decision", "old-decision"))]), "reference_duplicate")

    def test_mutated_dropped_or_drifted_history_refuses_against_anchor(self):
        code = "recovery_installation_history_rewrite"
        for phase in PHASES:
            with self.subTest(phase=phase):
                self.reject(lambda r, e: set_events(e, phase, [event(1, kind="policy_change")]), code)
                self.reject(lambda r, e: set_events(e, phase, []), code)
                self.reject(lambda r, e: edit_history(e, phase, lambda h: h.update(generation="5")), code)
                self.reject(lambda r, e: edit_history(e, phase, lambda h: h.update(
                    started_at="2025-12-31T22:00:00Z")), code)
        # Generation or events drifting between the two recovery checkpoints.
        self.reject(lambda r, e: edit_history(e, "final", lambda h: h.update(generation="5")), code)
        def seeded_history(r, e):
            events = [event(1), event(2, observed="2025-12-31T23:40:20Z")]
            edit_anchor_history(e, lambda h: h.update(events=copy.deepcopy(events), last_sequence=2))
            for phase in PHASES:
                set_events(e, phase, events)
        self.accept(seeded_history)
        def final_drift(r, e):
            seeded_history(r, e)
            edit_history(e, "final", lambda h: h["events"][1].update(
                observed_at="2025-12-31T23:40:21Z"))
        self.reject(final_drift, code)
        # The anchor drifting is equally visible to every later history.
        self.reject(lambda r, e: edit_anchor_history(e, lambda h: h.update(generation="5")), code)
        self.reject(lambda r, e: edit_anchor_history(e, lambda h: h["events"][0].update(
            kind="scope_change")), code)

    def test_backfilled_history_refuses(self):
        code = "recovery_installation_history_backfill"
        backfill = event(2, observed="2025-12-31T23:40:30Z")
        # An event inserted after the anchor ended, but dated before it, is backfill.
        self.reject(lambda r, e: set_events(e, "cleanup", [event(1), backfill]), code)
        self.reject(lambda r, e: set_events(e, "final", [event(1), backfill]), code)
        self.reject(lambda r, e: (set_events(e, "cleanup", [event(1), backfill]),
                                  set_events(e, "final", [event(1), backfill, event(
                                      3, observed="2025-12-31T23:40:40Z")])), code)

    def test_identical_extra_history_in_every_record_is_still_not_backfill_free(self):
        # An extra event must also be present identically in the anchor to be a prefix.
        extra = event(2, observed="2025-12-31T23:40:30Z")
        def everywhere(r, e):
            edit_anchor_history(e, lambda h: h.update(events=[event(1), extra], last_sequence=2))
            for phase in PHASES:
                set_events(e, phase, [event(1), extra])
        self.accept(everywhere)

    # Multiple cleanup operations and checkpoints.
    def test_second_operation_in_one_checkpoint_keeps_installation_claims(self):
        def same_checkpoint(r, e):
            add_second_cleanup(r, e)
            set_until(e, "cleanup", "00:20:41")
        self.accept(same_checkpoint)

    def test_second_checkpoint_needs_matching_installation_records(self):
        def split(r, e):
            add_second_cleanup(r, e, split_checkpoint=True)
        def with_install(r, e):
            split(r, e)
            add_install(e, "second", "00:20:35")
            inventory(e).insert(1, {"checkpoint_ref": "recovery-checkpoint-second",
                                    "installation_ref": approval_ref("second")})
        self.accept(with_install)
        self.reject(split, "recovery_installation_inventory")
        def inventory_without_records(r, e):
            split(r, e)
            inventory(e).insert(1, {"checkpoint_ref": "recovery-checkpoint-second",
                                    "installation_ref": approval_ref("second")})
        self.reject(inventory_without_records, "unresolved_reference")

    def test_each_second_checkpoint_installation_is_checked(self):
        def build(r, e):
            add_second_cleanup(r, e, split_checkpoint=True)
            add_install(e, "second", "00:20:35")
            inventory(e).insert(1, {"checkpoint_ref": "recovery-checkpoint-second",
                                    "installation_ref": approval_ref("second")})
        def then(change):
            def mutate(r, e):
                build(r, e)
                change(r, e)
            return mutate
        self.reject(then(lambda r, e: approval(e, "second").update(checked_at=at("00:20:10"))),
                    "recovery_installation_checkpoint")
        self.reject(then(lambda r, e: set_install_time(e, "second", "00:20:36")),
                    "stale_recovery_installation_read")
        self.reject(then(lambda r, e: approval(e, "second").update(
            controller_read_ref=install_read("cleanup", "controller"))),
            "recovery_installation_replay")
        self.reject(then(lambda r, e: inventory(e)[1].update(
            installation_ref=approval_ref("cleanup"))), "reference_duplicate")
        self.reject(then(lambda r, e: approval(e, "second")["decisions"][1].update(round=3)),
                    "recovery_installation_approval")
        self.reject(then(lambda r, e: edit_history(e, "second", lambda h: h.update(generation="5"))),
                    "recovery_installation_history_rewrite")
        self.reject(then(lambda r, e: edit_history(e, "second", lambda h: h.update(coverage="gap"))),
                    "recovery_installation_history")


if __name__ == "__main__":
    unittest.main()
