"""Synthetic responses for the exact PRE Worker lookup."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_target import Resource, Target, TargetError
    from pre_worker_lookup import WorkerLookupResponse, read_reserved_worker


TARGET = Target("pre-fixture-lookup", 42, 1_800_000_000, "0473/0465")
ACCOUNT = "a" * 32
PATH = f"/accounts/{ACCOUNT}/workers/workers/two-bot-next-pre-42-1800003600"
IDENTITY = {"id": "b" * 32, "name": "two-bot-next-pre-42-1800003600"}
SUCCESS = {"success": True, "errors": [], "messages": [], "result": IDENTITY}
MISSING_ERROR = {"code": 10007, "message": "Worker not found."}
MISSING = {"success": False, "errors": [MISSING_ERROR], "messages": [], "result": None}


class PreWorkerLookupTests(unittest.TestCase):
    def read(self, reader, *, source=TARGET, account=ACCOUNT):
        return read_reserved_worker(TARGET, source, staging_account_id=account, read=reader)

    def test_get_uses_derived_path_and_returns_exact_worker(self):
        reader = Mock(return_value=WorkerLookupResponse(PATH, 200, SUCCESS))
        self.assertEqual(self.read(reader), Resource(TARGET.worker_name, TARGET.worker_name))
        reader.assert_called_once_with(PATH)

    def test_canonical_not_found_proves_exact_worker_absence(self):
        reader = Mock(return_value=WorkerLookupResponse(PATH, 404, MISSING))
        self.assertIsNone(self.read(reader))
        reader.assert_called_once_with(PATH)

    def test_wrong_reservation_or_invalid_account_refuses_before_read(self):
        reader = Mock()
        for source in (None, replace(TARGET, scope="pre-fixture-other"),
                       replace(TARGET, shape="0475/0465"), replace(TARGET, run_id=43),
                       replace(TARGET, started_at=TARGET.started_at - 1)):
            with self.subTest(source=source), self.assertRaisesRegex(TargetError, "^wrong_pre_worker_target$"):
                self.read(reader, source=source)
        for account in (None, "", "staging", ACCOUNT + "/workers"):
            with self.subTest(account=account), self.assertRaisesRegex(TargetError, "^invalid_pre_worker_account$"):
                self.read(reader, account=account)
        reader.assert_not_called()

    def test_not_found_for_wrong_scope_route_or_worker_remains_unknown(self):
        for path in (PATH.replace(ACCOUNT, "c" * 32), PATH.replace("/workers/workers/", "/workers/scripts/"),
                     PATH + "-shadow", PATH + "?name=other", f"/accounts/{ACCOUNT}/workers/workers"):
            with self.subTest(path=path), self.assertRaisesRegex(TargetError, "^wrong_pre_worker_reply$"):
                self.read(Mock(return_value=WorkerLookupResponse(path, 404, MISSING)))

    def test_mismatched_or_malformed_identity_remains_unknown(self):
        for result in (None, [], [IDENTITY, IDENTITY], {"id": IDENTITY["id"]},
                       IDENTITY | {"name": TARGET.worker_name + "-shadow"},
                       IDENTITY | {"name": "two-bot-next-staging"},
                       IDENTITY | {"id": None}, IDENTITY | {"id": "  "}, IDENTITY | {"id": "bad\n"}):
            with self.subTest(result=result), self.assertRaises(TargetError):
                self.read(Mock(return_value=WorkerLookupResponse(PATH, 200, SUCCESS | {"result": result})))

    def test_bare_status_auth_route_and_ambiguous_replies_remain_unknown(self):
        auth = {"code": 10023, "message": "Not authorized."}
        route = {"code": 7003, "message": "No route for the URI."}
        for status, body in (
            (404, None), (404, {}), (404, "Not Found"), (404, {"errors": [MISSING_ERROR]}),
            (401, MISSING), (403, MISSING), (429, MISSING), (500, MISSING),
            (302, MISSING), (200, MISSING), (404.0, MISSING), (404, SUCCESS),
            (404, MISSING | {"errors": [auth]}), (404, MISSING | {"errors": [route]}),
            (404, MISSING | {"errors": [MISSING_ERROR, auth]}), (404, MISSING | {"errors": [MISSING_ERROR] * 2}),
            (404, MISSING | {"errors": [{"code": 10007}]}),
            (404, MISSING | {"errors": [MISSING_ERROR | {"code": "10007"}]}),
            (404, MISSING | {"errors": [MISSING_ERROR | {"code": 10007.0}]}),
            (404, MISSING | {"errors": [MISSING_ERROR | {"message": " "}]}),
            (404, MISSING | {"success": 0}), (404, MISSING | {"result": IDENTITY}),
            (404, MISSING | {"messages": [auth]}),
            (200, SUCCESS | {"success": 1}), (200, SUCCESS | {"errors": [auth]}),
            (200, SUCCESS | {"messages": None}), (200, SUCCESS | {"result_info": {"truncated": True}}),
        ):
            with self.subTest(status=status, body=body), self.assertRaises(TargetError):
                self.read(Mock(return_value=WorkerLookupResponse(PATH, status, body)))

    def test_read_failure_is_unknown_and_redacted(self):
        reader = Mock(side_effect=ValueError("synthetic private transport detail"))
        with self.assertRaisesRegex(TargetError, "^pre_worker_read_failed$"):
            self.read(reader)
        reader.assert_called_once_with(PATH)
