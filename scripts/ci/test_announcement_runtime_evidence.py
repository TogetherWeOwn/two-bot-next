"""Hermetic typed-sidecar intake tests; no actual authority or observations."""

import copy
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import announcement_runtime_evidence as evidence
from announcement_runtime_schema import ARCHIVE_LIMIT, RECEIPT_LIMIT, Refusal
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_fixtures import D1, D2, H1, U1, run


def sidecar(ref="document-one"):
    # Hand-written, synthetic claims, not copied from a schema specification.
    return {"schema": "two.bot.announcement-evidence.v1", "ref": ref,
            "kind": "document", "run": run(),
            "data": {"revision": U1, "sha256": D1}}


def files(record=None):
    record = sidecar() if record is None else record
    return {"evidence/" + record["ref"] + ".json": json.dumps(record).encode("utf-8")}


class TypedEvidence(unittest.TestCase):
    def test_resolve_snapshot_and_exact_pin_is_not_runtime_authority(self):
        records = files()
        index = evidence.EvidenceIndex(records, run())
        records.clear()
        claim = {"ref": "document-one", "revision": U1, "sha256": D1}
        index.bind(claim, "document")
        first = index.resolve("document-one", "document")
        first["sha256"] = D2
        self.assertEqual(index.resolve("document-one", "document"),
                         {"revision": U1, "sha256": D1})
        index.require_all_used()
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_interaction_plan_ids_fit_nonzero_u64_at_sidecar_intake(self):
        interaction = {"case_id": "off-rsvp", "command": "rsvp", "phase": "off",
            "interaction_id": "1", "application_id": "2", "actor_ref": "actor",
            "channel_ref": "channel", "event_ref": "event", "request_sha256": D1,
            "ack_count": 1, "edit_count": 0, "followup_count": 0, "ephemeral": True,
            "maximum_wire_attempts": 1}
        for field in ("interaction_id", "application_id"):
            for value in ("1", str(2**64 - 1), str(2**64), "99999999999999999999"):
                with self.subTest(field=field, value=value):
                    record = sidecar("interaction-plan")
                    record.update(kind="interaction_plan", data={
                        "interactions": [{**interaction, field: value}],
                        "exchanges": {"ref": "exchanges", "sha256": D1}})
                    if int(value) < 2**64:
                        index = evidence.EvidenceIndex(files(record), run())
                        actual = index.resolve("interaction-plan", "interaction_plan")
                        self.assertEqual(value, actual["interactions"][0][field])
                    else:
                        with self.assertRaisesRegex(Refusal, "discord_id$"):
                            evidence.EvidenceIndex(files(record), run())

    def test_manifest_and_document_are_not_interchangeable(self):
        record = sidecar("manifest-one")
        record.update(kind="manifest", data={"revision": H1, "sha256": D1})
        index = evidence.EvidenceIndex(files(record), run())
        index.bind({"ref": "manifest-one", "revision": H1, "sha256": D1}, "manifest")
        with self.assertRaisesRegex(Refusal, "reference_kind"):
            index.resolve("manifest-one", "document")

    def test_unknown_missing_or_wrong_kind_never_falls_back(self):
        index = evidence.EvidenceIndex(files(), run())
        for ref, kind in (("missing", "document"), ("document-one", "manifest"),
                          ("document-one", "authority_root")):
            with self.subTest(ref=ref, kind=kind), self.assertRaises(Refusal):
                index.resolve(ref, kind)
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence"):
            index.require_all_used()

    def test_no_open_or_unsupported_schema_records(self):
        mutations = [
            lambda r: r.update(extra=True),
            lambda r: r["data"].update(extra=True),
            lambda r: r.update(schema="two.bot.announcement-evidence.v2"),
            lambda r: r.update(kind="authority_root"),
            lambda r: r.update(kind="arbitrary_record"),
            lambda r: r.update(kind=[]),
            lambda r: r.update(data={}),
            lambda r: r.update(data=None),
            lambda r: r.update(ref="unknown"),
        ]
        for mutate in mutations:
            record = sidecar()
            mutate(record)
            with self.subTest(mutate=mutate), self.assertRaises(Refusal):
                evidence.EvidenceIndex(files(record), run())

    def test_filename_is_exact_ref_not_an_alias(self):
        for path in ("evidence/other.json", "candidate-receipt.json", "../document-one.json",
                     "evidence/document-one.JSON", "evidence/document-one.json\x00suffix"):
            with self.subTest(path=path), self.assertRaisesRegex(Refusal, "evidence_path"):
                evidence.EvidenceIndex({path: json.dumps(sidecar()).encode()}, run())

    def test_same_attempt_binding_at_intake_and_use(self):
        for key, value in (("run_id", "2"), ("attempt", 2), ("control_sha", H1)):
            record = sidecar()
            record["run"][key] = value
            with self.subTest(key=key), self.assertRaisesRegex(Refusal, "evidence_attempt"):
                evidence.EvidenceIndex(files(record), run())
        index = evidence.EvidenceIndex(files(), run())
        other = run()
        other["attempt"] = 2
        with self.assertRaisesRegex(Refusal, "evidence_attempt"):
            index.require_run(other)

    def test_pins_compare_revision_and_digest_not_just_name(self):
        for changed in ({"revision": "2" * 8 + "-" + U1[9:]}, {"sha256": D2}):
            claim = {"ref": "document-one", "revision": U1, "sha256": D1, **changed}
            with self.subTest(changed=changed), self.assertRaisesRegex(Refusal, "reference_binding"):
                evidence.EvidenceIndex(files(), run()).bind(claim, "document")

    def test_duplicate_keys_in_envelope_and_payload_refuse(self):
        raw = json.dumps(sidecar()).encode()
        for invalid in (raw.replace(b'"ref":', b'"ref":"other","ref":', 1),
                        raw.replace(b'"sha256":', b'"sha256":"' + D2.encode() +
                                    b'","sha256":', 1)):
            with self.subTest(raw=invalid[:30]), self.assertRaisesRegex(Refusal, "duplicate_key"):
                evidence.EvidenceIndex({"evidence/document-one.json": invalid}, run())

    def test_parser_limits_are_shared_with_receipt_not_permissive_json(self):
        malformed = [b"", b"[]", b"null", b"NaN", b"{} trailing", b"\xff",
                     b'{"x":' + b"[" * 13 + b"0" + b"]" * 13 + b"}",
                     b'{"x":"' + b"x" * 4097 + b'"}',
                     b'{"x":"\\ud800"}', b'{"x":1.0}',
                     b'{"x":123456789012345678901}',
                     b" " * (RECEIPT_LIMIT + 1)]
        for raw in malformed:
            with self.subTest(size=len(raw)), self.assertRaises(Refusal):
                evidence.EvidenceIndex({"evidence/document-one.json": raw}, run())

    def test_all_expanded_sidecars_have_one_total_byte_budget(self):
        # Refuse before decoding any oversized aggregate, even if each member
        # individually fits its receipt-sized bound.
        raw = b" " * RECEIPT_LIMIT
        records = {f"evidence/r{i}.json": raw for i in range(33)}
        self.assertGreater(sum(map(len, records.values())), ARCHIVE_LIMIT)
        with patch.object(evidence, "parse_bounded_json", side_effect=AssertionError("decoded")):
            with self.assertRaisesRegex(Refusal, "evidence_size"):
                evidence.EvidenceIndex(records, run())

    def test_empty_nonbytes_and_large_inventory_refuse(self):
        for records in ({}, [], {"evidence/document-one.json": "{}"},
                        {"evidence/document-one.json": bytearray(b"{}")},
                        {f"evidence/r{i}.json": b"{}" for i in range(257)}):
            with self.subTest(records=type(records)), self.assertRaises(Refusal):
                evidence.EvidenceIndex(records, run())

    def test_extra_even_valid_evidence_is_not_silently_accepted(self):
        packet = files()
        packet.update(files(sidecar("unused-document")))
        index = evidence.EvidenceIndex(packet, run())
        index.resolve("document-one", "document")
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence"):
            index.require_all_used()
        index.resolve("unused-document", "document")
        index.require_all_used()

    def test_invalid_errors_do_not_expose_rejected_body(self):
        sentinel = "private-synthetic-sentinel"
        raw = json.dumps({**sidecar(), "data": {"secret": sentinel}}).encode()
        with self.assertRaises(Refusal) as raised:
            evidence.EvidenceIndex({"evidence/document-one.json": raw}, run())
        self.assertNotIn(sentinel, str(raised.exception))
        self.assertNotIn(sentinel, repr(raised.exception))

    def test_byte_snapshot_is_not_a_mutable_caller_record(self):
        record = sidecar()
        packet = files(record)
        index = evidence.EvidenceIndex(packet, run())
        changed_run = copy.deepcopy(run())
        changed_run["run_id"] = "5"
        record.update(data={"revision": U1, "sha256": D2}, run=changed_run)
        self.assertEqual(index.resolve("document-one", "document")["sha256"], D1)


if __name__ == "__main__":
    unittest.main()
