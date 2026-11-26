"""Synthetic responses for the exact PRE Container application GET."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_container_lookup import (
        ContainerAppReply, ContainerAppScope, confirm_reserved_container_configuration, read_reserved_container_app,
    )
    from pre_registry_cleanup import RegistryReservation
    from pre_target import Inventory, Resource, Target, TargetError, verify_cleanup


TARGET = Target("pre-fixture-container", 42, 1_800_000_000, "0473/0465")
SCOPE = ContainerAppScope(TARGET, "1" * 32, "12345678-1234-4234-8234-123456789abc")
SIBLING_APP_ID = "87654321-4321-4321-8321-cba987654321"
PATH = f"/accounts/{SCOPE.account_id}/containers/applications/{SCOPE.app_id}"
APP = {"account_id": SCOPE.account_id, "id": SCOPE.app_id, "name": TARGET.container_name}
PRESENT = ContainerAppReply(SCOPE, PATH, 200, {"success": True, "errors": [], "messages": [], "result": APP})
NAMESPACE_ID = "ab" * 16
REPOSITORY = "registry.invalid/staging/two-bot-next-attendance-predecessor-staging"
IMAGE = RegistryReservation(TARGET, f"{REPOSITORY}@sha256:" + "c" * 64)
CONFIGURED = APP | {
    "durable_objects": {"namespace_id": NAMESPACE_ID},
    "configuration": {"image": IMAGE.reference},
}
# The fixture attests application-handler provenance independently of this invented error code.
ABSENT = ContainerAppReply(SCOPE, PATH, 404, {
    "success": False, "errors": [{"code": 19001, "message": "synthetic app missing"}],
    "messages": [], "result": None,
}, application_not_found=True)


class PreContainerLookupTests(unittest.TestCase):
    def read(self, reply):
        reader = Mock(return_value=reply)
        try:
            return read_reserved_container_app(SCOPE, read=reader)
        finally:
            reader.assert_called_once_with(PATH)

    def confirm_configuration(self, reply):
        reader = Mock(return_value=reply)
        try:
            return confirm_reserved_container_configuration(
                SCOPE, namespace_id=NAMESPACE_ID, image=IMAGE, read=reader,
            )
        finally:
            reader.assert_called_once_with(PATH)

    def test_reads_pinned_account_and_uuid_at_exact_application_path(self):
        self.assertEqual(self.read(PRESENT), Resource(TARGET.container_name, SCOPE.app_id))

    def test_authoritative_not_found_still_requires_independent_pre_cleanup_evidence(self):
        inventory = Inventory(TARGET, None, self.read(ABSENT), instance_count=None,
                              assigned_count=None, write_access=False, complete=True, startup_settled=True)
        with self.assertRaisesRegex(TargetError, "^pre_cleanup_unconfirmed$"):
            verify_cleanup(TARGET, inventory)
        stopped = replace(inventory, instance_count=0, assigned_count=0)
        with self.assertRaisesRegex(TargetError, "^pre_startup_unsettled$"):
            verify_cleanup(TARGET, replace(stopped, startup_settled=False))
        with self.assertRaisesRegex(TargetError, "^pre_cleanup_unconfirmed$"):
            verify_cleanup(TARGET, replace(stopped, write_access=True))
        with self.assertRaisesRegex(TargetError, "^pre_cleanup_unconfirmed$"):
            verify_cleanup(TARGET, replace(stopped, container=self.read(PRESENT)))
        self.assertIsNone(verify_cleanup(TARGET, stopped))

    def test_wrong_or_ambiguous_read_scope_refuses_even_404(self):
        for changes in (
            {"scope": None},
            {"scope": replace(SCOPE, account_id="2" * 32)},
            {"scope": replace(SCOPE, app_id=SIBLING_APP_ID)},
            {"scope": replace(SCOPE, target=replace(TARGET, scope="pre-fixture-sibling"))},
            {"scope": replace(SCOPE, target=replace(TARGET, shape="0475/0465"))},
            {"scope": replace(SCOPE, target=replace(TARGET, run_id=43))},
            {"scope": replace(SCOPE, target=replace(TARGET, started_at=TARGET.started_at + 1))},
            {"path": PATH.rsplit("/", 1)[0]},
            {"path": PATH.replace(SCOPE.account_id, "2" * 32)},
            {"path": PATH.replace(SCOPE.app_id, SIBLING_APP_ID)},
            {"path": PATH + "?name=" + TARGET.container_name},
            {"status": "404"},
            {"application_not_found": 1},
        ):
            with self.subTest(changes=changes), self.assertRaisesRegex(TargetError, "^wrong_pre_container_reply$"):
                self.read(replace(ABSENT, **changes))

    def test_success_requires_exact_returned_account_uuid_and_name(self):
        for changes in ({"account_id": "2" * 32}, {"account_id": None}, {"id": SIBLING_APP_ID},
                        {"name": replace(TARGET, run_id=43).container_name}):
            with self.subTest(changes=changes), self.assertRaisesRegex(TargetError, "^wrong_pre_app_detail$"):
                self.read(replace(PRESENT, body=PRESENT.body | {"result": APP | changes}))

    def test_unclassified_404_and_other_provider_statuses_do_not_prove_absence(self):
        replies = [replace(ABSENT, application_not_found=False)]
        replies.extend(replace(ABSENT, status=status) for status in (200, 302, 401, 403, 429, 500, 503))
        replies.append(replace(PRESENT, application_not_found=True))
        for reply in replies:
            with self.subTest(reply=reply), self.assertRaisesRegex(TargetError, "^pre_container_lookup_unconfirmed$"):
                self.read(reply)

    def test_contradictory_and_malformed_error_envelopes_refuse(self):
        reason = "^(unrecognized_pre_container_reply|pre_container_lookup_unconfirmed)$"
        for body in (
            None, {}, ABSENT.body | {"success": True}, ABSENT.body | {"result": APP},
            ABSENT.body | {"errors": []}, ABSENT.body | {"errors": ABSENT.body["errors"] * 2},
            ABSENT.body | {"errors": [{"code": 7003, "message": "No route for the URI"}]},
            ABSENT.body | {"errors": [{"code": "19001", "message": "synthetic app missing"}]},
            ABSENT.body | {"errors": [{"code": 19001, "message": ""}]},
            ABSENT.body | {"messages": [{"code": 19002, "message": "synthetic warning"}]},
        ):
            with self.subTest(body=body), self.assertRaisesRegex(TargetError, reason):
                self.read(replace(ABSENT, body=body))

    def test_invalid_pins_refuse_and_transport_errors_are_redacted(self):
        with self.assertRaisesRegex(TargetError, "^invalid_pre_container_account$"):
            replace(SCOPE, account_id="../foreign-account")
        with self.assertRaisesRegex(TargetError, "^invalid_pre_app_pin$"):
            replace(SCOPE, app_id="../sibling-app")
        reader = Mock(side_effect=OSError("synthetic provider error contents"))
        with self.assertRaisesRegex(TargetError, "^invalid_pre_container_scope$"):
            read_reserved_container_app(None, read=reader)
        reader.assert_not_called()
        with self.assertRaisesRegex(TargetError, "^pre_container_read_failed$") as failure:
            read_reserved_container_app(SCOPE, read=reader)
        self.assertTrue(failure.exception.__suppress_context__)

    def test_readback_representation_omits_provider_body(self):
        self.assertNotIn("synthetic app missing", repr(ABSENT))

    def test_confirms_exact_configured_namespace_and_image(self):
        reply = replace(PRESENT, body=PRESENT.body | {"result": CONFIGURED})
        self.assertIsNone(self.confirm_configuration(reply))

    def test_refuses_missing_or_mismatched_configuration(self):
        for result, reason in (
            (APP, "pre_container_namespace_mismatch"),
            (CONFIGURED | {"durable_objects": {"namespace_id": "bd" * 16}}, "pre_container_namespace_mismatch"),
            (CONFIGURED | {"durable_objects": None}, "pre_container_namespace_mismatch"),
            (CONFIGURED | {"configuration": {}}, "pre_container_image_mismatch"),
            (CONFIGURED | {"configuration": None}, "pre_container_image_mismatch"),
            (CONFIGURED | {"configuration": {"image": IMAGE.reference.replace("registry.", "foreign.", 1)}},
             "pre_container_image_mismatch"),
            (CONFIGURED | {"configuration": {"image": REPOSITORY + ":latest"}}, "pre_container_image_mismatch"),
            (CONFIGURED | {"configuration": {"image": IMAGE.reference[:-1] + "d"}}, "pre_container_image_mismatch"),
        ):
            reply = replace(PRESENT, body=PRESENT.body | {"result": result})
            with self.subTest(result=result), self.assertRaisesRegex(TargetError, f"^{reason}$"):
                self.confirm_configuration(reply)

    def test_absent_app_cannot_confirm_configuration(self):
        with self.assertRaisesRegex(TargetError, "^pre_container_configuration_unconfirmed$"):
            self.confirm_configuration(ABSENT)

    def test_configuration_refuses_foreign_target_and_app(self):
        foreign = replace(TARGET, shape="0475/0465")
        reply = replace(PRESENT, body=PRESENT.body | {"result": CONFIGURED})
        reader = Mock(return_value=reply)
        with self.assertRaisesRegex(TargetError, "^wrong_pre_container_image_scope$"):
            confirm_reserved_container_configuration(
                SCOPE, namespace_id=NAMESPACE_ID, image=replace(IMAGE, target=foreign), read=reader,
            )
        reader.assert_not_called()
        with self.assertRaisesRegex(TargetError, "^wrong_pre_container_reply$"):
            self.confirm_configuration(replace(reply, scope=replace(SCOPE, target=foreign)))
        with self.assertRaisesRegex(TargetError, "^wrong_pre_app_detail$"):
            self.confirm_configuration(replace(reply, body=reply.body | {"result": CONFIGURED | {"id": SIBLING_APP_ID}}))
