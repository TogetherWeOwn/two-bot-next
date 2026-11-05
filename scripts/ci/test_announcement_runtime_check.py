"""Offline CLI intake/control-flow tests, NOT positive packet qualification.

Mocked comparisons below exercise composition and the terminal HOLD only. The
real partial provenance fixture must refuse; no complete valid graph, service
inventory, authenticated root, actual observation or runtime pass is claimed.
"""

import contextlib
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import announcement_runtime_check as checker
import announcement_runtime_fixtures as fixtures
from announcement_provenance_fixtures import NOW, evidence_files, provenance_fixture
from announcement_runtime_evidence import EVIDENCE_SCHEMA
from announcement_runtime_policy import CAPABILITY_GAPS
from announcement_runtime_schema import ARCHIVE_LIMIT, RECEIPT_LIMIT, Refusal

SCRIPT = Path(checker.__file__).resolve()
SENTINEL = "private-input-must-not-echo"


def packet(receipt=None, sidecars=None):
    receipt = fixtures.receipt() if receipt is None else receipt
    if sidecars is None:
        sidecars = {"evidence/check-doc.json": json.dumps({
            "schema": EVIDENCE_SCHEMA, "ref": "check-doc",
            "kind": "document", "run": receipt["attempt"]["run"],
            "data": {"revision": "11111111-1111-1111-1111-111111111111", "sha256": "1" * 64},
        }).encode()}
    files = {"candidate-receipt.json": json.dumps(receipt).encode(), **sidecars}
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w") as archive:
        for name, raw in files.items():
            info = zipfile.ZipInfo(name)
            info.external_attr = (stat.S_IFREG | 0o600) << 16
            archive.writestr(info, raw)
    inventory = {"run": copy.deepcopy(receipt["attempt"]["run"]), "members": [
        {"path": name, "size": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}
        for name, raw in files.items()]}
    return output.getvalue(), json.dumps(inventory).encode()


class ArchiveComposition(unittest.TestCase):
    def test_public_checks_are_all_explicitly_composed(self):
        self.assertEqual([check.__name__ for check in checker.CLAIM_CHECKS], [
            "check_provenance_claims", "check_authority_graph_claims",
            "check_resource_graph_claims", "check_input_graph_claims",
            "check_scheduler_graph_claims", "check_source_input_claims",
            "check_telemetry_claims", "check_serialization_claims",
            "check_transition_claims", "check_setting_claims", "check_case_effect_claims",
            "check_lifecycle_ledger_claims", "check_drain_claims", "check_interaction_exchange_claims",
            "check_feed_exchange_claims", "check_delivery_trace_claims", "check_rsvp_storage_claims"])

    def test_same_index_and_clock_flow_through_every_comparison(self):
        raw, inventory = packet()
        calls = []

        def compare(receipt, evidence, now):
            calls.append((receipt, evidence, now))
            evidence.resolve("check-doc", "document")

        with mock.patch.object(checker, "CLAIM_CHECKS", (compare,) * 17):
            self.assertIsNone(checker.check_archive_claims(raw, inventory, now=NOW))
        self.assertEqual(len(calls), 17)
        self.assertTrue(all(r is calls[0][0] and e is calls[0][1] and n is NOW
                            for r, e, n in calls))
        self.assertIs(calls[0][0]["promotable"], False)

    def test_first_failed_comparison_stops_later_comparisons(self):
        raw, inventory = packet()
        failure = mock.Mock(side_effect=Refusal("synthetic_refusal"))
        later = mock.Mock()
        with mock.patch.object(checker, "CLAIM_CHECKS", (failure, later)):
            with self.assertRaisesRegex(Refusal, "synthetic_refusal$"):
                checker.check_archive_claims(raw, inventory, now=NOW)
        failure.assert_called_once()
        later.assert_not_called()

    def test_final_all_used_is_mandatory_even_if_comparisons_return(self):
        raw, inventory = packet()
        with mock.patch.object(checker, "CLAIM_CHECKS", ()):
            with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
                checker.check_archive_claims(raw, inventory, now=NOW)

    def test_run_drift_refuses_before_comparisons(self):
        raw, inventory = packet()
        for field, value in (("run_id", "999"), ("attempt", 2), ("control_sha", "f" * 40)):
            meta = json.loads(inventory)
            meta["run"][field] = value
            compare = mock.Mock()
            with self.subTest(field=field), mock.patch.object(checker, "CLAIM_CHECKS", (compare,)):
                with self.assertRaisesRegex(Refusal, "evidence_attempt$"):
                    checker.check_archive_claims(raw, json.dumps(meta).encode(), now=NOW)
                compare.assert_not_called()

    def test_adverse_receipt_cannot_be_upgraded(self):
        for status in ("fail", "hold"):
            receipt = fixtures.receipt()
            receipt["result"] = {"status": status, "reason_codes": ["effect"],
                                  "discrepancy_refs": ["check-doc"]}
            raw, inventory = packet(receipt)
            compare = mock.Mock()
            with self.subTest(status=status), mock.patch.object(checker, "CLAIM_CHECKS", (compare,)):
                with self.assertRaisesRegex(Refusal, "adverse_result$"):
                    checker.check_archive_claims(raw, inventory, now=NOW)
                compare.assert_not_called()

    def test_metadata_is_closed_duplicate_safe_and_bounded(self):
        raw, inventory = packet()
        meta = json.loads(inventory)
        wrong = [b'{"run":{},"run":{},"members":[]}', b"[]", b"{}",
                 json.dumps(dict(meta, root=SENTINEL)).encode(),
                 json.dumps(dict(meta, members=[])).encode(),
                 json.dumps(dict(meta, members=meta["members"] * 129)).encode(),
                 b" " * (RECEIPT_LIMIT + 1)]
        for value in wrong:
            with self.subTest(length=len(value)), self.assertRaises(Refusal):
                checker.check_archive_claims(raw, value, now=NOW)

    def test_member_inventory_and_archive_bytes_are_both_checked(self):
        raw, inventory = packet()
        meta = json.loads(inventory)
        for mutation in ({"path": "promotion-receipt.json"}, {"size": True},
                         {"sha256": "0" * 64}, {"extra": SENTINEL}):
            changed = copy.deepcopy(meta)
            changed["members"][0].update(mutation)
            with self.subTest(mutation=mutation), self.assertRaises(Refusal):
                checker.check_archive_claims(raw, json.dumps(changed).encode(), now=NOW)
        with self.assertRaises(Refusal):
            checker.check_archive_claims(raw + b"unlisted", inventory, now=NOW)

    def test_partial_provenance_fixture_is_not_full_packet_success(self):
        receipt, records, now = provenance_fixture()
        raw, inventory = packet(receipt, evidence_files(records))
        with self.assertRaises(Refusal):
            checker.check_archive_claims(raw, inventory, now=now)

    def test_no_caller_clock_strings_or_naive_datetimes(self):
        raw, inventory = packet()
        for now in (None, "2026-01-01T00:25:00Z", NOW.replace(tzinfo=None)):
            with self.subTest(now=now), self.assertRaisesRegex(Refusal, "clock$"):
                checker.check_archive_claims(raw, inventory, now=now)


class CliHold(unittest.TestCase):
    def invoke(self, argv, *, patches=()):
        output, errors = io.StringIO(), io.StringIO()
        with contextlib.ExitStack() as stack:
            for patch in patches:
                stack.enter_context(patch)
            stack.enter_context(contextlib.redirect_stdout(output))
            stack.enter_context(contextlib.redirect_stderr(errors))
            result = checker.main(argv)
        self.assertEqual(result, 1)
        self.assertEqual(errors.getvalue(), "")
        self.assertNotIn(SENTINEL, output.getvalue())
        report = json.loads(output.getvalue())
        self.assertEqual(set(report), {"status", "runtime_admission", "reason", "capability_gaps"})
        self.assertEqual(report["status"], "hold")
        self.assertIs(report["runtime_admission"], False)
        self.assertEqual(report["capability_gaps"], [
            {"capability": gap.capability, "owner": gap.owner} for gap in CAPABILITY_GAPS])
        return report

    def test_bad_arguments_never_echo_private_values(self):
        for argv in ([], [SENTINEL], ["--root", SENTINEL],
                     ["--archive", SENTINEL], ["--arc", SENTINEL, "--inventory", SENTINEL],
                     ["--archive", SENTINEL, "--inventory", SENTINEL, "--enable"]):
            with self.subTest(argv=argv):
                self.assertEqual(self.invoke(argv)["reason"], "offline_claims_refused")

    def test_file_and_parse_errors_do_not_echo_paths_or_values(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / SENTINEL
            source.write_bytes(("invalid-" + SENTINEL).encode())
            for path in (source, source.with_suffix(".missing"), Path(directory)):
                with self.subTest(path=path):
                    report = self.invoke(["--archive", str(path), "--inventory", str(source)])
                    self.assertEqual(report["reason"], "offline_claims_refused")

    def test_unexpected_comparison_exception_is_adverse_without_traceback(self):
        report = self.invoke(["--archive", "unused", "--inventory", "unused"], patches=(
            mock.patch.object(checker, "_read_regular", return_value=b"unused"),
            mock.patch.object(checker, "check_archive_claims", side_effect=RuntimeError(SENTINEL))))
        self.assertEqual(report["reason"], "offline_checker_error")

    def test_coherent_mocked_claim_path_still_reaches_real_unsupported_gate(self):
        with mock.patch.object(checker, "runtime_preflight", wraps=checker.runtime_preflight) as gate:
            report = self.invoke(["--archive", "unused", "--inventory", "unused"], patches=(
                mock.patch.object(checker, "_read_regular", return_value=b"unused"),
                mock.patch.object(checker, "check_archive_claims", return_value=None)))
        gate.assert_called_once_with()
        self.assertEqual(report["reason"], "unsupported_runtime")

    def test_unexpected_preflight_exception_is_also_sanitized(self):
        report = self.invoke(["--archive", "unused", "--inventory", "unused"], patches=(
            mock.patch.object(checker, "_read_regular", return_value=b"unused"),
            mock.patch.object(checker, "check_archive_claims", return_value=None),
            mock.patch.object(checker, "runtime_preflight", side_effect=RuntimeError(SENTINEL))))
        self.assertEqual(report["reason"], "offline_checker_error")

    def test_accidental_preflight_return_cannot_enable_offline_cli(self):
        report = self.invoke(["--archive", "unused", "--inventory", "unused"], patches=(
            mock.patch.object(checker, "_read_regular", return_value=b"unused"),
            mock.patch.object(checker, "check_archive_claims", return_value=None),
            mock.patch.object(checker, "runtime_preflight", return_value=None)))
        self.assertEqual(report["reason"], "unsupported_runtime")

    def test_regular_input_reader_refuses_empty_large_link_and_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "input"
            source.write_bytes(b"1234")
            self.assertEqual(checker._read_regular(source, 4), b"1234")
            with self.assertRaises(Refusal):
                checker._read_regular(source, 3)
            source.write_bytes(b"")
            with self.assertRaises(Refusal):
                checker._read_regular(source, 4)
            link = root / "link"
            link.symlink_to(source)
            with self.assertRaises(OSError):
                checker._read_regular(link, 4)
            with self.assertRaises((OSError, Refusal)):
                checker._read_regular(root, 4)
            with source.open("wb") as stream:
                stream.truncate(ARCHIVE_LIMIT + 1)
            with self.assertRaises(Refusal):
                checker._read_regular(source, ARCHIVE_LIMIT)

    def test_real_cli_refuses_fifo_without_waiting_or_creating_artifacts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fifo, inventory = root / SENTINEL, root / "inventory.json"
            os.mkfifo(fifo)
            inventory.write_text("{}")
            before = set(root.iterdir())
            for optimized in (False, True):
                command = [sys.executable] + (["-O"] if optimized else [])
                result = subprocess.run(command + [str(SCRIPT), "--archive", str(fifo),
                    "--inventory", str(inventory)], capture_output=True, text=True, timeout=5,
                    env={"PATH": os.defpath, "PYTHONDONTWRITEBYTECODE": "1"})
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stderr, "")
                self.assertNotIn(SENTINEL, result.stdout)
                self.assertIs(json.loads(result.stdout)["runtime_admission"], False)
                self.assertEqual(set(root.iterdir()), before)


if __name__ == "__main__":
    unittest.main()
