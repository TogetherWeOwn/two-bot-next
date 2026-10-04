"""Offline staging-gate fixtures; stdlib only, no Cloudflare, Docker or waits."""

import argparse
import contextlib
import copy
from datetime import datetime, timezone
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import MagicMock, Mock, patch
from urllib.error import HTTPError, URLError


spec = importlib.util.spec_from_file_location(
    "staging_rollout", Path(__file__).with_name("staging_rollout.py")
)
rollout = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rollout)

STARTED = 1_800_000_000.0
VERSION = "12345678-1234-4234-8234-123456789abc"
OLD_VERSION = "87654321-4321-4321-8321-cba987654321"
REVISION = "a" * 40
BUILD_ID = "123456-2"
ACCOUNT = "b" * 32
NAMESPACE = "c" * 64  # DO namespaces are opaque hex, not Worker-version UUIDs.
APPLICATION_ID = "app_opaque-123"
OLD_ROLLOUT = "rollout_before-123"
NEW_ROLLOUT = "rollout_after-456"
REPOSITORY = f"registry.cloudflare.com/{ACCOUNT}/two-bot-next-twobotcontainer-staging"
TAG = REPOSITORY + ":12345678"
IMAGE = REPOSITORY + "@sha256:" + "d" * 64
OLD_IMAGE = REPOSITORY + "@sha256:" + "e" * 64
URL = "https://two-bot-next-staging.offline-fixture.workers.dev"
SENTINEL = "RAW_SECRET_SENTINEL_never_disclose_database_password"
APP_PATH = "/containers/applications"
ROWS_PATH = f"/containers/applications/{APPLICATION_ID}/rollouts?limit=100"
DETAIL_PATH = f"/containers/applications/{APPLICATION_ID}/rollouts/{NEW_ROLLOUT}"
NAMESPACE_PATH = f"/workers/scripts/two-bot-next-staging/versions/{VERSION}"
DEPLOYMENTS_PATH = "/workers/scripts/two-bot-next-staging/deployments"
ENVIRONMENT = {
    "GITHUB_SHA": REVISION,
    "GITHUB_RUN_ID": "123456",
    "GITHUB_RUN_ATTEMPT": "2",
    "STAGING_URL": URL,
    "CLOUDFLARE_ACCOUNT_ID": ACCOUNT,
    "CLOUDFLARE_API_TOKEN": SENTINEL,
}

# Deliberately different paths in the default, staging and production containers.
# The file need not exist at the resolved image/main paths: prepare only rewrites them.
CONFIG = '''name = "two-bot-next"
main = "src/index.ts"
[[containers]]
class_name = "TwoBotContainer"
image = "../Dockerfile.default"
max_instances = 1
[[env.staging.containers]]
class_name = "TwoBotContainer"
image = "../Dockerfile.staging"
max_instances = 1
[env.staging.version_metadata]
binding = "CF_VERSION_METADATA"
[[env.production.containers]]
class_name = "TwoBotContainer"
image = "../Dockerfile.production"
max_instances = 1
[env.production.vars]
PRESERVED = "production-fixture"
'''


def date(offset=0):
    return datetime.fromtimestamp(STARTED + offset, timezone.utc).isoformat()


def receipts():
    return [
        {
            "type": "wrangler-session", "version": 1, "wrangler_version": "4.147.0",
            "command_line_args": ["deploy", "--config", "staging-deploy.json", "--env", "staging"],
        },
        {
            "type": "deploy", "version": 1,
            "worker_name": "two-bot-next-staging",
            "wrangler_environment": "staging", "worker_name_overridden": False,
            "timestamp": date(), "version_id": VERSION,
        },
    ]


def real_shape_receipts():
    """Record shapes written by Wrangler 4.147.0 when `wrangler-action` drives a deploy.

    Field names follow workers-sdk `packages/wrangler/src/index.ts` (the `wrangler-session`
    entry, written for EVERY invocation) and its deploy output entry; values are
    synthetic. The first session is the action's `wrangler --version` probe.
    """
    log = "/home/runner/.config/.wrangler/logs/wrangler-2026-10-03_01-22-59_123.log"
    return [
        {"version": 1, "type": "wrangler-session", "wrangler_version": "4.147.0",
         "command_line_args": ["--version"], "log_file_path": log, "timestamp": date()},
        {"version": 1, "type": "wrangler-session", "wrangler_version": "4.147.0",
         "command_line_args": ["deploy", "--config", "/tmp/staging-deploy.json", "--env", "staging"],
         "log_file_path": log, "timestamp": date()},
        {"version": 1, "type": "deploy", "worker_name": "two-bot-next-staging",
         "worker_tag": "tag123", "version_id": VERSION, "targets": [URL],
         "worker_name_overridden": False, "wrangler_environment": "staging", "timestamp": date()},
    ]


def baseline():
    return {
        "started": STARTED, "application_id": APPLICATION_ID,
        "namespace_id": NAMESPACE, "rollout_ids": [OLD_ROLLOUT],
        "revision": REVISION, "build_id": BUILD_ID,
    }


def image_details():
    return {"revision": REVISION, "build_id": BUILD_ID, "digests": [IMAGE]}


def completed_row():
    return {
        "id": NEW_ROLLOUT, "created_at": date(),
        "strategy": "rolling", "kind": "full_auto", "status": "completed",
        "target_version": 8,
        "target_configuration": {"image": IMAGE, "env": {"DATABASE_URL": SENTINEL}},
        # Documented API semantics: these remain the BEFORE configuration.
        "current_version": 7,
        "current_configuration": {"image": OLD_IMAGE, "env": {"DATABASE_URL": SENTINEL}},
        "health": {"instances": {
            "active": 1, "healthy": 1, "failed": 0, "starting": 0, "scheduling": 0,
        }},
        "progress": {"total_steps": 2, "current_step": 2,
                     "total_instances": 1, "updated_instances": 1},
        "steps": [{"status": "completed"}, {"status": "completed"}],
    }


def old_row():
    row = completed_row()
    row.update(id=OLD_ROLLOUT, created_at=date(-100))
    row["target_configuration"]["image"] = OLD_IMAGE
    return row


def app(image=IMAGE):
    return {
        "id": APPLICATION_ID, "name": "two-bot-next-twobotcontainer-staging",
        "durable_objects": {"namespace_id": NAMESPACE},
        "configuration": {"image": image, "env": {"DATABASE_URL": SENTINEL}},
    }


def bindings():
    return {"resources": {"bindings": [{
        "name": "TWO_BOT", "class_name": "TwoBotContainer",
        "type": "durable_object_namespace", "namespace_id": NAMESPACE,
    }]}}


def deployment():
    return {"deployments": [{"versions": [{"version_id": VERSION, "percentage": 100}]}]}


def ready_response():
    body = {"build_revision": REVISION, "build_id": BUILD_ID,
            "components": [["database", "ready"], ["gateway", "ready"], ["http", "ready"]]}
    return 200, {"x-two-worker-version": VERSION}, json.dumps(body).encode()


class FakeClock:
    def __init__(self):
        self.now = 100.0
        self.sleeps = []

    def monotonic(self):
        return self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds


class FakeClient:
    """Explicit routes only; queued snapshots, repeating the final response."""

    def __init__(self, api_routes=None, request_routes=None, deadline=110.0):
        self.api_routes = copy.deepcopy(api_routes or {})
        self.request_routes = copy.deepcopy(request_routes or {})
        self.deadline = deadline
        self.observation = None
        self.calls = []

    def respond(self, routes, key):
        if key not in routes or not routes[key]:
            raise AssertionError(f"unexpected offline fixture route: {key}")
        values = routes[key]
        value = values.pop(0) if len(values) > 1 else values[0]
        if isinstance(value, Exception):
            raise value
        return copy.deepcopy(value)

    def api(self, path):
        self.calls.append(("api", path))
        return self.respond(self.api_routes, path)

    def request(self, url, authenticated=False):
        if authenticated:
            raise AssertionError("runtime probes must not send API credentials")
        self.calls.append(("request", url))
        return self.respond(self.request_routes, url)


def verify_client():
    return FakeClient(
        {
            APP_PATH: [[app()]], ROWS_PATH: [[old_row(), completed_row()]],
            DETAIL_PATH: [completed_row(), completed_row()],
            NAMESPACE_PATH: [bindings()], DEPLOYMENTS_PATH: [deployment(), deployment()],
        },
        {URL + "/readyz": [ready_response()],
         URL + "/health": [(200, {"x-two-worker-version": VERSION}, b"discarded")]},
    )


class OfflineTestCase(unittest.TestCase):
    def setUp(self):
        # Fail loudly if a new test accidentally tries real HTTP or Docker.
        opener = Mock()
        opener.open.side_effect = AssertionError("live HTTP forbidden in offline tests")
        self.enterContext(patch.object(rollout, "build_opener", return_value=opener))
        self.enterContext(patch.object(rollout.subprocess, "run",
                                      side_effect=AssertionError("live Docker forbidden")))

    def assert_gate(self, code, function, *args):
        with self.assertRaises(rollout.GateError) as caught:
            function(*args)
        self.assertEqual(str(caught.exception), code)
        self.assertNotIn(SENTINEL, str(caught.exception))


class DeployReceiptTests(OfflineTestCase):
    def test_valid_pinned_wrangler_deploy(self):
        self.assertEqual(rollout.deploy_version(receipts(), STARTED), VERSION)

    def test_ndjson_round_trip_with_unrelated_records(self):
        records = receipts() + [{"type": "build", "message": SENTINEL}]
        ndjson = "\n".join(json.dumps(record) for record in records)
        self.assertEqual(rollout.deploy_version(
            [rollout.mapping(rollout.decode(line)) for line in ndjson.encode().splitlines()], STARTED
        ), VERSION)

    def test_stale_deploy(self):
        records = receipts()
        records[1]["timestamp"] = date(-1)
        self.assert_gate("stale_deploy_receipt", rollout.deploy_version, records, STARTED)

    def test_wrong_environment_worker_override_and_schema(self):
        for key, value in [("wrangler_environment", "production"),
                           ("worker_name", "two-bot-next"),
                           ("worker_name_overridden", True),
                           ("worker_name_overridden", None), ("version", 2)]:
            with self.subTest(key=key, value=value):
                records = receipts()
                records[1][key] = value
                self.assert_gate("wrong_deploy_receipt", rollout.deploy_version, records, STARTED)

    def test_missing_or_ambiguous_deploy(self):
        for records in [receipts()[:1], receipts() + [receipts()[1]]]:
            with self.subTest(count=len(records)):
                self.assert_gate("deploy_receipt_missing_or_ambiguous",
                                 rollout.deploy_version, records, STARTED)

    def test_wrangler_action_version_probe_is_accepted_beside_the_deploy_session(self):
        # wrangler-action runs `wrangler --version` first; Wrangler records that too.
        for flag in ("--version", "-v"):
            with self.subTest(flag=flag):
                probe = {"type": "wrangler-session", "version": 1,
                         "wrangler_version": "4.147.0", "command_line_args": [flag]}
                self.assertEqual(rollout.deploy_version([probe] + receipts(), STARTED), VERSION)
                self.assertEqual(rollout.deploy_version(receipts() + [probe], STARTED), VERSION)

    def test_missing_ambiguous_or_wrong_wrangler_session(self):
        wrong = receipts()
        wrong[0]["wrangler_version"] = "4.142.0"
        schema = receipts()
        schema[0]["version"] = 2
        no_args = receipts()
        del no_args[0]["command_line_args"]
        bad_args = receipts()
        bad_args[0]["command_line_args"] = ["deploy", 1]
        probe = {"type": "wrangler-session", "version": 1,
                 "wrangler_version": "4.147.0", "command_line_args": ["--version"]}
        wrong_probe = dict(probe, wrangler_version="4.142.0")
        schema_probe = dict(probe, version=2)
        other_command = dict(probe, command_line_args=["secret", "put", "X"])
        no_args_probe = {key: value for key, value in probe.items() if key != "command_line_args"}
        for records in [receipts()[1:], receipts() + [receipts()[0]], wrong, schema, no_args,
                        bad_args, receipts()[1:] + [probe],
                        [wrong_probe] + receipts(), [schema_probe] + receipts(),
                        [other_command] + receipts(), [no_args_probe] + receipts()]:
            with self.subTest(records=records):
                self.assert_gate("wrong_wrangler_receipt", rollout.deploy_version, records, STARTED)

    def test_failure_diagnostic_is_allowlisted_counts_and_versions_only(self):
        records = real_shape_receipts()
        records[0]["wrangler_version"] = "4.142.0"
        records[1]["wrangler_version"] = SENTINEL
        records.append({"type": SENTINEL, "command_line_args": [SENTINEL]})
        with self.assertRaises(rollout.GateError) as caught:
            rollout.deploy_version(records, STARTED)
        self.assertEqual(str(caught.exception), "wrong_wrangler_receipt")
        self.assertEqual(caught.exception.detail,
                         "records deploy=1,other=1,wrangler-session=2; "
                         "sessions deploy=1,probe=1,other=0; "
                         "v1/wrangler-4.142.0 v1/wrangler-invalid")
        for private in [SENTINEL, "command_line_args", "log_file_path", "/home/runner", "timestamp"]:
            self.assertNotIn(private, caught.exception.detail)

    def test_diagnostic_names_session_class_counts(self):
        # An unrecognized session (e.g. another Wrangler command at the pinned
        # version) must show up as `other` so the failing sub-check is named.
        records = real_shape_receipts()
        records[1]["command_line_args"] = ["secret", "put", "X"]
        with self.assertRaises(rollout.GateError) as caught:
            rollout.deploy_version(records, STARTED)
        self.assertEqual(str(caught.exception), "wrong_wrangler_receipt")
        self.assertIn("sessions deploy=0,probe=1,other=1", caught.exception.detail)
        self.assertNotIn("secret", caught.exception.detail)

    def test_diagnostic_does_not_print_non_integer_or_bool_versions(self):
        records = [{"type": "wrangler-session", "version": True, "wrangler_version": 4.1},
                   {"type": "wrangler-session", "version": 10 ** 6, "wrangler_version": None}]
        self.assertEqual(rollout.receipt_shape(records),
                         "records wrangler-session=2; sessions deploy=0,probe=0,other=2; "
                         "vinvalid/wrangler-invalid vinvalid/wrangler-invalid")
        self.assertEqual(rollout.receipt_shape([]),
                         "records none; sessions deploy=0,probe=0,other=0; none")

    def test_deploy_field_diagnostic_names_failed_checks_only(self):
        records = receipts()
        records[1]["wrangler_environment"] = SENTINEL
        with self.assertRaises(rollout.GateError) as caught:
            rollout.deploy_version(records, STARTED)
        self.assertEqual(str(caught.exception), "wrong_deploy_receipt")
        self.assertTrue(caught.exception.detail.endswith(
            "deploy version=ok worker=ok environment=bad not_overridden=ok"))
        self.assertNotIn(SENTINEL, caught.exception.detail)

    def test_missing_deploy_fields_fail_closed(self):
        for key in ["worker_name", "wrangler_environment", "worker_name_overridden", "version"]:
            with self.subTest(key=key):
                records = receipts()
                del records[1][key]
                self.assert_gate("wrong_deploy_receipt", rollout.deploy_version, records, STARTED)

    def test_invalid_missing_or_naive_timestamp(self):
        for value in [None, "not-a-date", "2027-01-15T08:00:00", 1]:
            with self.subTest(value=value):
                records = receipts()
                records[1]["timestamp"] = value
                self.assert_gate("invalid_timestamp", rollout.deploy_version, records, STARTED)

    def test_worker_version_must_be_uuid_even_though_other_ids_are_opaque(self):
        for version in [NAMESPACE, APPLICATION_ID, "12345678", VERSION.upper()]:
            with self.subTest(version=version):
                records = receipts()
                records[1]["version_id"] = version
                self.assert_gate("invalid_worker_version", rollout.deploy_version, records, STARTED)

    def test_unsafe_deploy_identity(self):
        records = receipts()
        records[1]["version_id"] = "../" + SENTINEL
        self.assert_gate("invalid_identity", rollout.deploy_version, records, STARTED)


class ImageReceiptTests(OfflineTestCase):
    def test_exact_repo_tag_digest_revision_and_nonce(self):
        tags = [TAG, "unrelated:latest", REPOSITORY + ":87654321"]
        self.assertEqual(rollout.image_receipt(tags, image_details(), VERSION, REVISION, BUILD_ID), IMAGE)

    def test_stale_image_revision_or_build_nonce(self):
        for key, value in [("revision", "f" * 40), ("build_id", "123456-1"),
                           ("revision", None), ("build_id", None)]:
            with self.subTest(key=key, value=value):
                details = image_details()
                details[key] = value
                self.assert_gate("image_build_mismatch", rollout.image_receipt,
                                 [TAG], details, VERSION, REVISION, BUILD_ID)

    def test_wrong_repo_exact_tag_and_ambiguous_tags(self):
        for tags in [[], [REPOSITORY + ":87654321"], [TAG + "-extra"],
                     [TAG.replace("-staging", "-production")],
                     [TAG.replace("registry.cloudflare.com", "untrusted.example")],
                     [TAG, TAG], [TAG, TAG.replace(ACCOUNT, "f" * 32)]]:
            with self.subTest(tags=tags):
                self.assert_gate("image_tag_missing_or_ambiguous", rollout.image_receipt,
                                 tags, image_details(), VERSION, REVISION, BUILD_ID)

    def test_digest_must_match_selected_exact_repository(self):
        details = image_details()
        details["digests"] = [IMAGE.replace(ACCOUNT, "f" * 32)]
        self.assert_gate("image_digest_missing_or_ambiguous", rollout.image_receipt,
                         [TAG], details, VERSION, REVISION, BUILD_ID)

    def test_missing_ambiguous_or_malformed_digest(self):
        for digests in [[], [IMAGE, OLD_IMAGE], [IMAGE, IMAGE], [IMAGE[:-1]],
                        [IMAGE.replace("sha256:", "sha512:")], [TAG], [None],
                        [IMAGE.replace("-staging", "-production")]]:
            with self.subTest(digests=digests):
                details = image_details()
                details["digests"] = digests
                self.assert_gate("image_digest_missing_or_ambiguous", rollout.image_receipt,
                                 [TAG], details, VERSION, REVISION, BUILD_ID)

    def test_unknown_digest_schema(self):
        for value in [None, IMAGE, {"digest": IMAGE}]:
            with self.subTest(value=value):
                details = image_details()
                details["digests"] = value
                self.assert_gate("invalid_api_schema", rollout.image_receipt,
                                 [TAG], details, VERSION, REVISION, BUILD_ID)

    def test_docker_inspects_only_allowlisted_labels_and_digests(self):
        outputs = [subprocess.CompletedProcess([], 0, (TAG + "\n").encode()),
                   subprocess.CompletedProcess([], 0, json.dumps(image_details()).encode())]
        with patch.object(rollout.subprocess, "run", side_effect=outputs) as run:
            self.assertEqual(rollout.docker_image(VERSION, REVISION, BUILD_ID), IMAGE)
        self.assertEqual(run.call_count, 2)
        inspect = run.call_args_list[1].args[0]
        self.assertEqual(inspect[:4], ["docker", "image", "inspect", TAG])
        self.assertIn("org.opencontainers.image.revision", inspect[-1])
        self.assertIn("com.togetherweown.build-id", inspect[-1])
        self.assertIn(".RepoDigests", inspect[-1])
        self.assertNotIn(".Config.Env", inspect[-1])
        for call in run.call_args_list:
            self.assertEqual(call.kwargs, {"capture_output": True, "timeout": 20, "check": True})

    def test_docker_failure_does_not_expose_stderr(self):
        error = subprocess.CalledProcessError(1, ["docker"], output=SENTINEL, stderr=SENTINEL)
        with patch.object(rollout.subprocess, "run", side_effect=error):
            self.assert_gate("image_inspection_failed", rollout.docker_image, VERSION, REVISION, BUILD_ID)


class RolloutSelectionTests(OfflineTestCase):
    def test_baseline_ids_never_count_even_if_healthy_and_exact_image(self):
        row = completed_row()
        row["id"] = OLD_ROLLOUT
        self.assertIsNone(rollout.select_rollout([row], baseline(), IMAGE))
        # Baseline rows need not have current timestamps or target shape.
        self.assertIsNone(rollout.select_rollout([{"id": OLD_ROLLOUT}], baseline(), IMAGE))

    def test_exact_new_row_selected_alongside_baseline(self):
        row = completed_row()
        self.assertIs(rollout.select_rollout([old_row(), row], baseline(), IMAGE), row)

    def test_no_new_rollout_returns_none(self):
        self.assertIsNone(rollout.select_rollout([], baseline(), IMAGE))

    def test_multiple_new_rows_fail(self):
        other = completed_row()
        other["id"] = "another_rollout"
        self.assert_gate("ambiguous_new_rollout", rollout.select_rollout,
                         [completed_row(), other], baseline(), IMAGE)

    def test_competing_image_fails_even_with_one_exact_candidate(self):
        other = completed_row()
        other["id"] = "competing_rollout"
        other["target_configuration"]["image"] = OLD_IMAGE
        for rows in [[other], [completed_row(), other], [other, completed_row()]]:
            with self.subTest(order=[row["id"] for row in rows]):
                self.assert_gate("competing_rollout", rollout.select_rollout, rows, baseline(), IMAGE)

    def test_stale_new_rollout_is_not_new_evidence(self):
        row = completed_row()
        row["created_at"] = date(-1)
        self.assert_gate("stale_new_rollout", rollout.select_rollout, [row], baseline(), IMAGE)

    def test_missing_or_unknown_row_shape(self):
        for row, code in [(None, "invalid_api_schema"), ({}, "invalid_identity"),
                          ({"id": NEW_ROLLOUT}, "invalid_timestamp"),
                          ({"id": NEW_ROLLOUT, "created_at": date()}, "invalid_api_schema"),
                          ({"id": NEW_ROLLOUT, "created_at": date(), "target_configuration": {}},
                           "competing_rollout")]:
            with self.subTest(row=row):
                self.assert_gate(code, rollout.select_rollout, [row], baseline(), IMAGE)


class ConvergenceTests(OfflineTestCase):
    def test_completed_rolling_full_auto_accepts_before_current_configuration(self):
        row = completed_row()
        self.assertNotEqual(row["current_version"], row["target_version"])
        self.assertNotEqual(row["current_configuration"]["image"], IMAGE)
        self.assertTrue(rollout.converged(row, IMAGE, 8))

    def test_pending_and_progressing_are_not_complete(self):
        for status in ["pending", "progressing"]:
            with self.subTest(status=status):
                row = completed_row()
                row["status"] = status
                self.assertFalse(rollout.converged(row, IMAGE, 8))

    def test_unknown_reverted_and_replaced_status(self):
        for status, code in [("reverted", "rollout_replaced_or_reverted"),
                             ("replaced", "rollout_replaced_or_reverted"),
                             ("failed", "unknown_rollout_status"),
                             ("success", "unknown_rollout_status"), (None, "unknown_rollout_status")]:
            with self.subTest(status=status):
                row = completed_row()
                row["status"] = status
                self.assert_gate(code, rollout.converged, row, IMAGE, 8)

    def test_wrong_or_missing_strategy_and_kind(self):
        for key, value in [("strategy", "immediate"), ("kind", "manual"),
                           ("strategy", None), ("kind", None)]:
            with self.subTest(key=key, value=value):
                row = completed_row()
                row[key] = value
                self.assert_gate("unsupported_rollout_profile", rollout.converged, row, IMAGE, 8)

    def test_target_version_or_image_drift(self):
        row = completed_row()
        row["target_version"] = 9
        self.assert_gate("rollout_identity_drift", rollout.converged, row, IMAGE, 8)
        row = completed_row()
        row["target_configuration"]["image"] = OLD_IMAGE
        self.assert_gate("rollout_identity_drift", rollout.converged, row, IMAGE, 8)

    def test_missing_counters_fail_not_default_to_zero(self):
        for key in ["active", "healthy", "failed", "starting", "scheduling"]:
            with self.subTest(key=key):
                row = completed_row()
                del row["health"]["instances"][key]
                self.assert_gate("invalid_api_schema", rollout.converged, row, IMAGE, 8)

    def test_wrong_counter_types_reject_bool_string_float_and_negative(self):
        for value in [True, "1", 1.0, -1, None]:
            with self.subTest(value=value):
                row = completed_row()
                row["health"]["instances"]["healthy"] = value
                self.assert_gate("invalid_api_schema", rollout.converged, row, IMAGE, 8)

    def test_failed_health_or_warming_instances_do_not_converge(self):
        for key, value in [("failed", 1), ("healthy", 0), ("active", 0),
                           ("active", 2), ("starting", 1), ("scheduling", 1)]:
            with self.subTest(key=key, value=value):
                row = completed_row()
                row["health"]["instances"][key] = value
                self.assertFalse(rollout.converged(row, IMAGE, 8))

    def test_incomplete_instance_progress(self):
        for total, updated in [(1, 0), (0, 0), (2, 1), (2, 2)]:
            with self.subTest(total=total, updated=updated):
                row = completed_row()
                row["progress"].update(total_instances=total, updated_instances=updated)
                self.assertFalse(rollout.converged(row, IMAGE, 8))

    def test_incomplete_steps(self):
        for status in ["pending", "progressing", "failed", None]:
            with self.subTest(status=status):
                row = completed_row()
                row["steps"][0]["status"] = status
                self.assert_gate("rollout_steps_incomplete", rollout.converged, row, IMAGE, 8)

    def test_invalid_step_progress(self):
        for update in [{"total_steps": 0}, {"total_steps": 3}, {"current_step": 3}]:
            with self.subTest(update=update):
                row = completed_row()
                row["progress"].update(update)
                self.assert_gate("invalid_rollout_progress", rollout.converged, row, IMAGE, 8)

    def test_missing_progress_counters(self):
        for key in ["total_steps", "current_step", "total_instances", "updated_instances"]:
            with self.subTest(key=key):
                row = completed_row()
                del row["progress"][key]
                self.assert_gate("invalid_api_schema", rollout.converged, row, IMAGE, 8)

    def test_missing_or_unknown_completed_schema(self):
        for key in ["target_version", "target_configuration", "current_version",
                    "current_configuration", "health", "progress", "steps"]:
            with self.subTest(missing=key):
                row = completed_row()
                del row[key]
                self.assert_gate("invalid_api_schema", rollout.converged, row, IMAGE, 8)
        for key, value in [("health", []), ("progress", []), ("steps", {}),
                           ("current_configuration", []), ("current_version", True)]:
            with self.subTest(unknown=key):
                row = completed_row()
                row[key] = value
                self.assert_gate("invalid_api_schema", rollout.converged, row, IMAGE, 8)


def lag_row():
    row = completed_row()
    row["health"]["instances"]["active"] = 0
    return row


class ActiveLagTests(OfflineTestCase):
    def test_completed_zero_active_with_healthy_singleton_is_lag_not_converged(self):
        self.assertFalse(rollout.converged(lag_row(), IMAGE, 8))
        self.assertTrue(rollout.active_lag(lag_row(), IMAGE, 8))

    def test_pending_and_progressing_are_never_lag(self):
        for status in ["pending", "progressing"]:
            with self.subTest(status=status):
                row = lag_row()
                row["status"] = status
                self.assertFalse(rollout.active_lag(row, IMAGE, 8))

    def test_any_other_counter_deviation_is_not_lag(self):
        for key, value in [("active", 1), ("healthy", 0), ("failed", 1),
                           ("starting", 1), ("scheduling", 1)]:
            with self.subTest(key=key, value=value):
                row = lag_row()
                row["health"]["instances"][key] = value
                self.assertFalse(rollout.active_lag(row, IMAGE, 8))

    def test_lag_shares_converged_identity_and_schema_checks(self):
        row = lag_row()
        row["target_version"] = 9
        self.assert_gate("rollout_identity_drift", rollout.active_lag, row, IMAGE, 8)
        row = lag_row()
        row["status"] = "replaced"
        self.assert_gate("rollout_replaced_or_reverted", rollout.active_lag, row, IMAGE, 8)
        row = lag_row()
        del row["health"]["instances"]["active"]
        self.assert_gate("invalid_api_schema", rollout.active_lag, row, IMAGE, 8)

    def test_lag_needs_completed_steps_and_instance_progress(self):
        row = lag_row()
        row["steps"][0]["status"] = "pending"
        self.assert_gate("rollout_steps_incomplete", rollout.active_lag, row, IMAGE, 8)
        row = lag_row()
        row["progress"].update(total_instances=1, updated_instances=0)
        self.assertFalse(rollout.active_lag(row, IMAGE, 8))


class RuntimeTests(OfflineTestCase):
    def test_all_components_ready_for_exact_worker_sha_and_build(self):
        self.assertTrue(rollout.runtime_ready(*ready_response(), VERSION, REVISION, BUILD_ID))

    def test_503_and_500_never_pass_even_with_exact_ready_body(self):
        _, headers, body = ready_response()
        for status in [503, 500, 0, 204, 302]:
            with self.subTest(status=status):
                self.assertFalse(rollout.runtime_ready(status, headers, body, VERSION, REVISION, BUILD_ID))
                self.assertFalse(rollout.runtime_ready(status, {}, SENTINEL.encode(),
                                                       VERSION, REVISION, BUILD_ID))

    def test_old_or_missing_serving_worker_fails(self):
        status, _, body = ready_response()
        for headers in [{}, {"x-two-worker-version": OLD_VERSION}]:
            with self.subTest(headers=headers):
                self.assert_gate("serving_worker_mismatch", rollout.runtime_ready,
                                 status, headers, body, VERSION, REVISION, BUILD_ID)

    def test_old_sha_or_build_nonce_fails(self):
        status, headers, body = ready_response()
        for key, value in [("build_revision", "f" * 40), ("build_id", "123456-1"),
                           ("build_revision", None), ("build_id", None)]:
            with self.subTest(key=key, value=value):
                report = json.loads(body)
                report[key] = value
                self.assert_gate("serving_image_mismatch", rollout.runtime_ready,
                                 status, headers, json.dumps(report).encode(), VERSION, REVISION, BUILD_ID)

    def test_empty_unready_or_malformed_components_never_pass(self):
        for components in [[], [["database", "ready"], ["gateway", "warming"]],
                           [["database", "failed"]], [["database"]],
                           [["database", "ready", "extra"]], ["ready"], [{"state": "ready"}]]:
            with self.subTest(components=components):
                status, headers, body = ready_response()
                report = json.loads(body)
                report["components"] = components
                self.assertFalse(rollout.runtime_ready(status, headers, json.dumps(report).encode(),
                                                       VERSION, REVISION, BUILD_ID))

    def test_missing_components_and_invalid_json_fail_closed(self):
        status, headers, body = ready_response()
        report = json.loads(body)
        del report["components"]
        self.assert_gate("invalid_api_schema", rollout.runtime_ready,
                         status, headers, json.dumps(report).encode(), VERSION, REVISION, BUILD_ID)
        self.assert_gate("invalid_json", rollout.runtime_ready,
                         status, headers, SENTINEL.encode(), VERSION, REVISION, BUILD_ID)


class IdentityAndControlPlaneTests(OfflineTestCase):
    def test_opaque_hex_and_safe_non_uuid_identifiers_supported(self):
        for value in [NAMESPACE, APPLICATION_ID, NEW_ROLLOUT, VERSION, "A_9-z", "a" * 128]:
            with self.subTest(value=value):
                self.assertEqual(rollout.identifier(value), value)
        client = FakeClient({APP_PATH: [[app()]], NAMESPACE_PATH: [bindings()]})
        self.assertEqual(rollout.application(client)["durable_objects"]["namespace_id"], NAMESPACE)
        self.assertEqual(rollout.worker_namespace(client, VERSION), NAMESPACE)

    def test_unsafe_ids_fail(self):
        for value in [None, 1, "", "a" * 129, "../id", "a/b", "a\\b", "id?query",
                      "id#fragment", "id%2fsecret", "id space", "id\n", "id@host", "é"]:
            with self.subTest(value=value):
                self.assert_gate("invalid_identity", rollout.identifier, value)

    def test_missing_ambiguous_or_unsafe_application(self):
        bad = app()
        bad["durable_objects"]["namespace_id"] = "../unsafe"
        for rows, code in [([], "application_identity_missing_or_ambiguous"),
                           ([app(), app()], "application_identity_missing_or_ambiguous"),
                           ([bad], "invalid_identity")]:
            with self.subTest(code=code, count=len(rows)):
                self.assert_gate(code, rollout.application, FakeClient({APP_PATH: [rows]}))

    def test_rollout_snapshot_truncation_and_duplicate_ids(self):
        many = [{"id": f"row_{index}"} for index in range(100)]
        for rows, code in [(many, "rollout_snapshot_truncated"),
                           ([old_row(), old_row()], "duplicate_rollout_identity")]:
            with self.subTest(code=code):
                self.assert_gate(code, rollout.rollouts, FakeClient({ROWS_PATH: [rows]}), APPLICATION_ID)

    def test_worker_binding_missing_ambiguous_or_wrong_class(self):
        wrong = bindings()
        wrong["resources"]["bindings"][0]["class_name"] = "OtherContainer"
        duplicate = bindings()
        duplicate["resources"]["bindings"] *= 2
        for result in [{"resources": {"bindings": []}}, wrong, duplicate]:
            with self.subTest(result=result):
                self.assert_gate("worker_namespace_missing_or_ambiguous", rollout.worker_namespace,
                                 FakeClient({NAMESPACE_PATH: [result]}), VERSION)

    def test_active_worker_must_be_single_exact_version_at_100_percent(self):
        self.assertIsNone(rollout.active_worker(FakeClient({DEPLOYMENTS_PATH: [deployment()]}), VERSION))
        for versions in [[{"version_id": OLD_VERSION, "percentage": 100}],
                         [{"version_id": VERSION, "percentage": 50}],
                         [{"version_id": VERSION, "percentage": 100},
                          {"version_id": OLD_VERSION, "percentage": 0}], []]:
            with self.subTest(versions=versions):
                data = {"deployments": [{"versions": versions}]}
                self.assert_gate("worker_version_not_active", rollout.active_worker,
                                 FakeClient({DEPLOYMENTS_PATH: [data]}), VERSION)


class ClientSanitizationTests(OfflineTestCase):
    def test_authenticated_http_and_transport_errors_have_fixed_codes(self):
        for error, code in [(HTTPError("https://offline.invalid", 500, SENTINEL, {},
                                      io.BytesIO(SENTINEL.encode())), "api_http_failure"),
                            (URLError(SENTINEL), "api_transport_failure"),
                            (TimeoutError(SENTINEL), "api_transport_failure"),
                            (OSError(SENTINEL), "api_transport_failure")]:
            with self.subTest(code=code, error_type=type(error).__name__):
                client = rollout.Client(ACCOUNT, SENTINEL)
                client.opener.open.side_effect = error
                self.assert_gate(code, client.api, "/containers/applications")

    def test_unauthenticated_http_error_discards_body(self):
        client = rollout.Client(ACCOUNT, SENTINEL)
        client.opener.open.side_effect = HTTPError(
            URL, 503, SENTINEL, {}, io.BytesIO(SENTINEL.encode())
        )
        self.assertEqual(client.request(URL + "/readyz"), (503, {}, b""))

    def test_unauthenticated_503_keeps_the_body_only_when_it_is_json(self):
        body = b'{"gateway_failure":{"phase":"durable_gateway","class":"checkpoint_load_failed"}}'
        for headers in [{"content-type": "application/json"},
                        {"content-type": "Application/JSON; charset=utf-8"}]:
            with self.subTest(headers=headers):
                client = rollout.Client(ACCOUNT, SENTINEL)
                client.opener.open.side_effect = HTTPError(URL, 503, "x", headers, io.BytesIO(body))
                self.assertEqual(client.request(URL + "/readyz"), (503, headers, body))
        for code, headers in [(503, {"content-type": "text/plain"}), (503, {}),
                              (500, {"content-type": "application/json"}),
                              (429, {"content-type": "application/json"})]:
            with self.subTest(code=code, headers=headers):
                client = rollout.Client(ACCOUNT, SENTINEL)
                client.opener.open.side_effect = HTTPError(
                    URL, code, SENTINEL, headers, io.BytesIO(SENTINEL.encode()))
                self.assertEqual(client.request(URL + "/readyz"), (code, {}, b""))

    def test_unauthenticated_json_503_body_is_size_bounded_and_read_errors_discard_it(self):
        client = rollout.Client(ACCOUNT, SENTINEL)
        big = io.BytesIO(b" " * (rollout.MAX_BODY + 1))
        client.opener.open.side_effect = HTTPError(
            URL, 503, "x", {"content-type": "application/json"}, big)
        self.assertEqual(client.request(URL + "/readyz"), (503, {}, b""))
        broken = Mock()
        broken.read.side_effect = OSError(SENTINEL)
        client.opener.open.side_effect = HTTPError(
            URL, 503, "x", {"content-type": "application/json"}, broken)
        self.assertEqual(client.request(URL + "/readyz"), (503, {}, b""))

    def test_api_error_envelope_does_not_expose_upstream_errors(self):
        client = rollout.Client(ACCOUNT, SENTINEL)
        body = json.dumps({"success": False, "errors": [{"message": SENTINEL}],
                           "result": app()}).encode()
        with patch.object(client, "request", return_value=(200, {}, body)):
            self.assert_gate("api_failure", client.api, "/containers/applications")

    def test_bad_api_status_json_and_schema_fail(self):
        client = rollout.Client(ACCOUNT, SENTINEL)
        for status, body, code in [(500, SENTINEL.encode(), "api_http_failure"),
                                   (200, SENTINEL.encode(), "invalid_json"),
                                   (200, b"[]", "invalid_api_schema"),
                                   (200, b'{"success":true}', "api_failure")]:
            with self.subTest(status=status, code=code):
                with patch.object(client, "request", return_value=(status, {}, body)):
                    self.assert_gate(code, client.api, "/containers/applications")

    def test_runtime_request_never_sends_api_token(self):
        client = rollout.Client(ACCOUNT, SENTINEL)
        response = Mock(status=200, headers={})
        response.read.return_value = b"{}"
        client.opener.open.side_effect = None
        client.opener.open.return_value = MagicMock()
        client.opener.open.return_value.__enter__.return_value = response
        client.request(URL + "/health")
        request = client.opener.open.call_args.args[0]
        self.assertIsNone(request.get_header("Authorization"))
        client.request(client.base + APP_PATH, authenticated=True)
        request = client.opener.open.call_args.args[0]
        self.assertEqual(request.get_header("Authorization"), "Bearer " + SENTINEL)


class DeploymentWiringTests(unittest.TestCase):
    def test_checked_in_staging_config_has_version_metadata_and_single_container(self):
        root = Path(__file__).resolve().parent.parent
        config = tomllib.loads((root / "wrangler" / "wrangler.toml").read_text())
        self.assertEqual(config["name"], "two-bot-next")
        staging = config["env"]["staging"]
        self.assertEqual(staging["version_metadata"]["binding"], "CF_VERSION_METADATA")
        self.assertEqual(len(staging["containers"]), 1)
        self.assertEqual(staging["containers"][0]["class_name"], "TwoBotContainer")
        self.assertEqual(staging["containers"][0]["max_instances"], 1)

    def test_deploy_workflow_requires_prepare_then_deploy_then_verify(self):
        # Source assertions deliberately avoid adding a non-stdlib YAML parser.
        root = Path(__file__).resolve().parent.parent
        source = (root / ".github" / "workflows" / "deploy-staging.yml").read_text()
        steps = re.split(r"(?m)^      - ", source)[1:]
        prepare = [step for step in steps if "python3 ../scripts/staging_rollout.py prepare" in step]
        deploy = [step for step in steps if "command: deploy --config" in step]
        receipt = [step for step in steps if "python3 ../scripts/staging_rollout.py receipt" in step]
        takeover = [step for step in steps if "ownership-control.mjs deployment-takeover" in step]
        verify = [step for step in steps if "python3 ../scripts/staging_rollout.py verify" in step]
        self.assertEqual((len(prepare), len(deploy), len(receipt), len(takeover), len(verify)),
                         (1, 1, 1, 1, 1))
        # The local Wrangler receipt is accepted after the deploy and before ownership moves.
        self.assertLess(steps.index(deploy[0]), steps.index(receipt[0]))
        self.assertLess(steps.index(receipt[0]), steps.index(takeover[0]))
        self.assertLess(steps.index(takeover[0]), steps.index(verify[0]))
        self.assertNotRegex(receipt[0], r"(?m)^\s*(?:if|continue-on-error):")
        self.assertIn("set -euo pipefail", receipt[0])
        self.assertIn('--receipt "$ROLLOUT_DIR/baseline.json"', receipt[0])
        self.assertIn('--output "$WRANGLER_OUTPUT_FILE_PATH"', receipt[0])
        # Network-free: no Cloudflare credentials or staging URL in this step.
        self.assertNotIn("secrets.", receipt[0])
        self.assertNotIn("STAGING_URL", receipt[0])
        self.assertNotIn("|| true", receipt[0])
        # GitHub rejects the whole workflow if job-level env uses the runner context.
        job_header = source.split("    steps:\n", 1)[0]
        self.assertNotIn("runner.", job_header)
        paths = [step for step in steps if 'echo "ROLLOUT_DIR=' in step]
        self.assertEqual(len(paths), 1)
        self.assertIn('$RUNNER_TEMP/', paths[0])
        self.assertIn('>> "$GITHUB_ENV"', paths[0])
        self.assertIn('echo "WRANGLER_OUTPUT_FILE_PATH=$rollout_dir/', paths[0])
        self.assertLess(steps.index(paths[0]), steps.index(prepare[0]))
        self.assertLess(steps.index(prepare[0]), steps.index(deploy[0]))
        self.assertLess(steps.index(deploy[0]), steps.index(verify[0]))
        for step in [prepare[0], deploy[0], verify[0]]:
            self.assertNotRegex(step, r"(?m)^\s*(?:if|continue-on-error):")
        for step in [prepare[0], verify[0]]:
            self.assertIn("set -euo pipefail", step)
            self.assertIn("STAGING_URL: ${{ vars.STAGING_WORKER_URL }}", step)
            self.assertIn('--receipt "$ROLLOUT_DIR/baseline.json"', step)
            self.assertIn('--output "$WRANGLER_OUTPUT_FILE_PATH"', step)
            self.assertNotIn("|| true", step)
        self.assertIn('wranglerVersion: "4.147.0"', deploy[0])
        self.assertIn("--env staging", deploy[0])
        self.assertIn('${{ env.ROLLOUT_DIR }}/staging-deploy.json', deploy[0])
        self.assertIn('--evidence "$ROLLOUT_DIR/evidence.json"', verify[0])
        executable_lines = "\n".join(line for line in source.splitlines()
                                      if line.strip() and not line.lstrip().startswith("#"))
        self.assertNotRegex(executable_lines, r"\b503\b")
        self.assertNotIn("curl", executable_lines)

    def test_required_worker_ci_runs_this_offline_suite(self):
        root = Path(__file__).resolve().parent.parent
        source = (root / ".github" / "workflows" / "check.yml").read_text()
        worker = source.split("  worker:\n", 1)[1]
        self.assertIn("name: worker check", worker)
        steps = re.split(r"(?m)^      - ", worker)[1:]
        matching = [step for step in steps if "test_staging_rollout.py" in step]
        self.assertEqual(len(matching), 1)
        self.assertRegex(matching[0], r"python3\s+(?:\.\./scripts/test_staging_rollout\.py|"
                         r"-m unittest discover -s \.\./scripts -p 'test_staging_rollout\.py')")
        self.assertNotRegex(matching[0], r"(?m)^\s*(?:if|continue-on-error):")
        self.assertNotIn("|| true", matching[0])


class OrchestrationTests(OfflineTestCase):
    def setUp(self):
        super().setUp()
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.config = self.root / "wrangler" / "wrangler.toml"
        self.config.parent.mkdir()
        self.config.write_text(CONFIG)
        self.args = argparse.Namespace(
            config=str(self.config), deploy_config=str(self.root / "deploy.json"),
            receipt=str(self.root / "baseline.json"), output=str(self.root / "deploy.ndjson"),
            evidence=str(self.root / "evidence.json"),
        )
        self.clock = FakeClock()
        self.enterContext(patch.dict(os.environ, ENVIRONMENT, clear=True))
        self.enterContext(patch.object(rollout.time, "time", return_value=STARTED))
        self.enterContext(patch.object(rollout.time, "monotonic", side_effect=self.clock.monotonic))
        self.enterContext(patch.object(rollout.time, "sleep", side_effect=self.clock.sleep))
        self.stdout = self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.stderr = self.enterContext(contextlib.redirect_stderr(io.StringIO()))
        self.docker = self.enterContext(patch.object(rollout, "docker_image", return_value=IMAGE))

    def prepare_baseline(self):
        client = FakeClient({APP_PATH: [[app(OLD_IMAGE)]], ROWS_PATH: [[old_row()]]})
        rollout.prepare(self.args, client)
        return client

    def write_deploy_output(self, records=None):
        records = receipts() if records is None else records
        # Verify's NDJSON reader must tolerate blank lines, not stale extra sessions.
        Path(self.args.output).write_text("\n" + "\n\n".join(json.dumps(row) for row in records) + "\n")

    def assert_no_secret_saved_or_printed(self):
        self.assertNotIn(SENTINEL, self.stdout.getvalue())
        self.assertNotIn(SENTINEL, self.stderr.getvalue())
        for path in self.root.rglob("*"):
            if path.is_file():
                self.assertNotIn(SENTINEL, path.read_text(), str(path))

    def assert_no_evidence(self):
        self.assertFalse(Path(self.args.evidence).exists())

    def test_prepare_records_only_exact_allowlisted_baseline(self):
        client = self.prepare_baseline()
        self.assertEqual(json.loads(Path(self.args.receipt).read_text()), baseline())
        self.assertEqual(client.calls, [("api", APP_PATH), ("api", ROWS_PATH)])
        self.assertFalse(Path(self.args.output).exists())
        self.assert_no_evidence()
        self.assert_no_secret_saved_or_printed()

    def test_generated_main_and_first_default_and_all_env_build_paths(self):
        self.prepare_baseline()
        generated = json.loads(Path(self.args.deploy_config).read_text())
        self.assertEqual(generated["main"], str(self.config.parent / "src" / "index.ts"))
        containers = [generated["containers"][0], generated["env"]["staging"]["containers"][0],
                      generated["env"]["production"]["containers"][0]]
        for container, suffix in zip(containers, ["default", "staging", "production"]):
            with self.subTest(suffix=suffix):
                self.assertEqual(container["image"], str(self.root / f"Dockerfile.{suffix}"))
                self.assertEqual(container["image_build_context"], str(self.root))
        staging = containers[1]
        self.assertEqual(staging["image_vars"], {"BOT_BUILD_REVISION": REVISION, "BOT_BUILD_ID": BUILD_ID})
        self.assertEqual(staging["rollout_kind"], "full_auto")
        for container in [containers[0], containers[2]]:
            self.assertNotIn("image_vars", container)
            self.assertNotIn("rollout_kind", container)
        self.assertEqual(generated["env"]["production"]["vars"], {"PRESERVED": "production-fixture"})
        self.assertEqual(self.config.read_text(), CONFIG)

    def test_main_prepare_uses_default_fixture_config_and_generated_paths(self):
        # argparse's first/default config and output paths, not the repository's config.
        default_config = self.root / "wrangler.toml"
        default_config.write_text(CONFIG)
        client = FakeClient({APP_PATH: [[app(OLD_IMAGE)]], ROWS_PATH: [[old_row()]]})
        argv = ["staging_rollout.py", "prepare", "--receipt", "default-receipt.json",
                "--output", "default-deploy.ndjson"]
        with contextlib.chdir(self.root), patch.object(sys, "argv", argv), \
                patch.object(rollout, "Client", return_value=client):
            self.assertEqual(rollout.main(), 0)
        generated = json.loads((self.root / "staging-deploy.json").read_text())
        self.assertEqual(generated["main"], str(self.root / "src" / "index.ts"))
        self.assertEqual(generated["containers"][0]["image"],
                         str((self.root / "../Dockerfile.default").resolve()))
        self.assertEqual(generated["containers"][0]["image_build_context"], str(self.root.parent))
        self.assertEqual(json.loads((self.root / "default-receipt.json").read_text()), baseline())
        self.assert_no_secret_saved_or_printed()

    def test_prepare_rejects_missing_production_and_credentialed_url_before_writes(self):
        for url in ["", "https://two-bot-next.offline-fixture.workers.dev",
                    "https://user:password@" + URL[8:], "http://" + URL[8:]]:
            with self.subTest(url=url):
                client = FakeClient()
                with patch.dict(os.environ, {"STAGING_URL": url}):
                    self.assert_gate("invalid_staging_url", rollout.prepare, self.args, client)
                self.assertEqual(client.calls, [])
                self.assertFalse(Path(self.args.receipt).exists())
                self.assertFalse(Path(self.args.deploy_config).exists())

    def test_prepare_rejects_stale_appended_output(self):
        Path(self.args.output).write_text("old deploy receipt")
        self.assert_gate("deploy_output_not_fresh", self.prepare_baseline)
        self.assertEqual(Path(self.args.output).read_text(), "old deploy receipt")
        self.assert_no_evidence()

    def test_prepare_rejects_prior_rollout_in_flight(self):
        row = old_row()
        row["status"] = "progressing"
        client = FakeClient({APP_PATH: [[app()]], ROWS_PATH: [[row]]})
        self.assert_gate("prior_rollout_in_flight", rollout.prepare, self.args, client)
        self.assertFalse(Path(self.args.receipt).exists())

    def test_prepare_invalid_build_identity_and_wrong_config(self):
        with patch.dict(os.environ, {"GITHUB_SHA": "bad-sha"}):
            self.assert_gate("invalid_build_identity", self.prepare_baseline)
        for config, code in [(CONFIG.replace('name = "two-bot-next"', 'name = "other"'),
                              "wrong_staging_config"),
                             (CONFIG.replace('binding = "CF_VERSION_METADATA"', 'binding = "OTHER"'),
                              "missing_worker_version_binding")]:
            with self.subTest(code=code):
                self.config.write_text(config)
                self.assert_gate(code, self.prepare_baseline)
        self.assertFalse(Path(self.args.receipt).exists())

    def test_success_exact_pinned_evidence_with_post_probe_rechecks(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        rollout.verify(self.args, client)
        self.assertEqual(json.loads(Path(self.args.evidence).read_text()), {
            "worker_version": VERSION, "application_id": APPLICATION_ID,
            "rollout_id": NEW_ROLLOUT, "target_version": 8, "image": IMAGE,
            "revision": REVISION, "build_id": BUILD_ID, "readyz": 200, "health": 200,
        })
        self.docker.assert_called_once_with(VERSION, REVISION, BUILD_ID)
        self.assertEqual(client.calls, [
            ("api", NAMESPACE_PATH), ("api", APP_PATH), ("api", ROWS_PATH),
            ("api", DETAIL_PATH), ("api", DEPLOYMENTS_PATH), ("request", URL + "/readyz"),
            ("request", URL + "/health"), ("api", DEPLOYMENTS_PATH), ("api", DETAIL_PATH),
            ("api", APP_PATH),
        ])
        self.assertEqual(self.clock.sleeps, [])
        self.assert_no_secret_saved_or_printed()

    def lag_client(self, readyz_routes):
        lagged = lag_row()
        client = verify_client()
        client.api_routes[ROWS_PATH] = [[old_row(), lagged]]
        client.api_routes[DETAIL_PATH] = [lagged]
        client.request_routes[URL + "/readyz"] = readyz_routes
        return client

    def test_active_lag_accepts_after_two_consecutive_full_passes(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = self.lag_client([ready_response()])
        rollout.verify(self.args, client)
        self.assertEqual(json.loads(Path(self.args.evidence).read_text()), {
            "worker_version": VERSION, "application_id": APPLICATION_ID,
            "rollout_id": NEW_ROLLOUT, "target_version": 8, "image": IMAGE,
            "revision": REVISION, "build_id": BUILD_ID, "readyz": 200, "health": 200,
            "active_lag": True,
        })
        self.assertEqual(self.clock.sleeps, [5])
        self.assert_no_secret_saved_or_printed()

    def test_active_lag_streak_resets_on_any_non_passing_poll(self):
        self.prepare_baseline()
        self.write_deploy_output()
        _, headers, body = ready_response()
        flapping = [ready_response(), (503, headers, body), ready_response(), ready_response()]
        client = self.lag_client(flapping)
        client.deadline = 120
        rollout.verify(self.args, client)
        self.assertTrue(json.loads(Path(self.args.evidence).read_text())["active_lag"])
        self.assertEqual(self.clock.sleeps, [5, 5, 5])
        self.assert_no_secret_saved_or_printed()

    def test_single_lag_pass_without_confirmation_times_out(self):
        self.prepare_baseline()
        self.write_deploy_output()
        _, headers, body = ready_response()
        client = self.lag_client([ready_response(), (503, headers, body)])
        self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
        self.assertTrue(client.observation.startswith("rollout=active_lag "))
        self.assert_no_evidence()
        self.assert_no_secret_saved_or_printed()

    def fresh_baseline(self):
        # `prepare` refuses a stale deploy output, so loops reset the fixtures.
        Path(self.args.output).unlink(missing_ok=True)
        self.prepare_baseline()
        self.write_deploy_output()

    def test_final_reread_accepts_counters_that_wobble_between_completed_shapes(self):
        # First read lag, final read converged (and the reverse): the exact
        # build served ready both times, so a counter wobble must not fail.
        for first, final, active_lag_expected in [
            (lag_row(), completed_row(), True),
            (completed_row(), lag_row(), False),
        ]:
            with self.subTest(first_is_lag=active_lag_expected):
                self.fresh_baseline()
                client = verify_client()
                client.api_routes[ROWS_PATH] = [[old_row(), first]]
                client.api_routes[DETAIL_PATH] = [first, final]
                if active_lag_expected:
                    client.api_routes[DETAIL_PATH] = [first, first, first, final]
                    client.request_routes[URL + "/readyz"] = [ready_response()]
                    client.deadline = 130
                rollout.verify(self.args, client)
                evidence = json.loads(Path(self.args.evidence).read_text())
                self.assertEqual(evidence.get("active_lag", False), active_lag_expected)
                Path(self.args.evidence).unlink()

    def test_final_reread_still_rejects_failed_starting_or_idle_counters(self):
        for key, value in [("failed", 1), ("starting", 1), ("scheduling", 1)]:
            with self.subTest(key=key):
                self.fresh_baseline()
                client = verify_client()
                bad = completed_row()
                bad["health"]["instances"][key] = value
                client.api_routes[DETAIL_PATH] = [completed_row(), bad]
                self.assert_gate("rollout_not_converged", rollout.verify, self.args, client)
                self.assert_no_evidence()
        idle = completed_row()
        idle["health"]["instances"].update(active=0, healthy=0)
        self.fresh_baseline()
        client = verify_client()
        client.api_routes[DETAIL_PATH] = [completed_row(), idle]
        self.assert_gate("rollout_not_converged", rollout.verify, self.args, client)
        self.assert_no_evidence()

    def test_lag_without_exact_runtime_never_succeeds(self):
        self.prepare_baseline()
        self.write_deploy_output()
        _, headers, body = ready_response()
        client = self.lag_client([(503, headers, body)])
        self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
        self.assert_no_evidence()
        self.assert_no_secret_saved_or_printed()

    def test_verify_ndjson_rejections_happen_before_image_or_api_probes(self):
        self.prepare_baseline()
        stale = receipts()
        stale[1]["timestamp"] = date(-1)
        wrong_env = receipts()
        wrong_env[1]["wrangler_environment"] = "production"
        wrong_wrangler = receipts()
        wrong_wrangler[0]["wrangler_version"] = "4.142.0"
        for records, code in [(stale, "stale_deploy_receipt"),
                              (wrong_env, "wrong_deploy_receipt"),
                              (receipts()[:1], "deploy_receipt_missing_or_ambiguous"),
                              (receipts() + [receipts()[1]], "deploy_receipt_missing_or_ambiguous"),
                              (wrong_wrangler, "wrong_wrangler_receipt"),
                              ([None], "invalid_api_schema")]:
            with self.subTest(code=code):
                self.write_deploy_output(records)
                client = verify_client()
                self.assert_gate(code, rollout.verify, self.args, client)
                self.assertEqual(client.calls, [])
                self.assert_no_evidence()
        self.docker.assert_not_called()

    def test_no_new_rollout_times_out_without_real_waits_or_evidence(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        # An otherwise exact, completed image from a baseline ID must never pass.
        old = completed_row()
        old["id"] = OLD_ROLLOUT
        client.api_routes[ROWS_PATH] = [[old]]
        self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
        self.assertEqual(self.clock.sleeps, [5, 5])
        self.assertEqual(self.clock.now, client.deadline)
        self.assertNotIn(("api", DETAIL_PATH), client.calls)
        self.assertNotIn(("request", URL + "/readyz"), client.calls)
        self.assert_no_evidence()
        self.assert_no_secret_saved_or_printed()

    def test_eventual_rollout_arrival_and_progress_keep_same_pinned_identity(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        client.deadline = 120
        client.api_routes[ROWS_PATH] = [[old_row()], [old_row(), completed_row()]]
        pending = completed_row()
        pending["status"] = "progressing"
        client.api_routes[DETAIL_PATH] = [pending, completed_row(), completed_row()]
        rollout.verify(self.args, client)
        self.assertEqual(self.clock.sleeps, [5, 5])
        self.assertEqual(client.calls.count(("api", ROWS_PATH)), 2)
        self.assertEqual(client.calls.count(("api", DETAIL_PATH)), 3)
        self.assertEqual(json.loads(Path(self.args.evidence).read_text())["rollout_id"], NEW_ROLLOUT)

    def test_non_200_readiness_never_succeeds_despite_healthy_health_endpoint(self):
        self.prepare_baseline()
        self.write_deploy_output()
        for status in [503, 500]:
            with self.subTest(status=status):
                self.clock.now = 100
                client = verify_client()
                _, headers, body = ready_response()
                client.request_routes[URL + "/readyz"] = [(status, headers, body)]
                self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
                self.assert_no_evidence()
        self.assert_no_secret_saved_or_printed()

    def test_timeout_records_which_stage_was_stuck_without_echoing_service_output(self):
        self.prepare_baseline()
        self.write_deploy_output()
        _, headers, _ = ready_response()
        stuck = {"build_revision": REVISION, "build_id": BUILD_ID,
                 "components": [["process", "ready"], ["gateway", "starting"],
                                ["token_invalid", "ready"], ["Bad\nname", "ready"],
                                ["gateway", SENTINEL], ["x"], "junk"]}
        for status, body, expected in [
            (503, json.dumps(stuck).encode(),
             "rollout=converged readyz=503 components=process:ready,gateway:starting,token_invalid:ready "
             "identity=match"),
            (503, json.dumps({**stuck, "build_id": "other"}).encode(),
             "rollout=converged readyz=503 components=process:ready,gateway:starting,token_invalid:ready "
             "identity=mismatch"),
            (503, SENTINEL.encode(), "rollout=converged readyz=503 body=unreadable"),
            (0, b"", "rollout=converged readyz=0 body=unreadable"),
        ]:
            with self.subTest(status=status, expected=expected):
                self.clock.now = 100
                client = verify_client()
                client.request_routes[URL + "/readyz"] = [(status, headers, body)]
                self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
                self.assertEqual(client.observation, expected)
                self.assertNotIn(SENTINEL, client.observation)
        self.assert_no_secret_saved_or_printed()

    def test_timeout_observation_for_missing_and_unconverged_rollouts(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        client.api_routes[ROWS_PATH] = [[old_row()]]
        self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
        self.assertEqual(client.observation, "no_new_rollout")
        self.clock.now = 100
        pending = completed_row()
        pending["status"] = "progressing"
        pending["health"] = {"instances": {"active": 1, "healthy": 0, "failed": 0, "starting": 1,
                                           "scheduling": SENTINEL}}
        client = verify_client()
        client.api_routes[DETAIL_PATH] = [pending]
        self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
        self.assertEqual(client.observation,
                         "rollout=progressing instances=active:1,healthy:0,failed:0,starting:1")
        self.assert_no_secret_saved_or_printed()

    def readyz_body(self, failure, **extra):
        body = {"build_revision": REVISION, "build_id": BUILD_ID,
                "components": [["process", "ready"], ["gateway", "down"]], **extra}
        if failure is not None:
            body["gateway_failure"] = failure
        return json.dumps(body).encode()

    def test_timeout_names_the_gateway_failure_class_from_a_converged_rollout(self):
        self.prepare_baseline()
        self.write_deploy_output()
        _, headers, _ = ready_response()
        failure = {"phase": "durable_gateway", "class": "checkpoint_load_failed"}
        self.clock.now = 100
        client = verify_client()
        client.request_routes[URL + "/readyz"] = [(503, headers, self.readyz_body(failure))]
        self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
        self.assertEqual(
            client.observation,
            "rollout=converged readyz=503 components=process:ready,gateway:down identity=match "
            "gateway_failure=durable_gateway:checkpoint_load_failed")

    def test_hostile_gateway_failure_values_are_dropped_from_the_observation(self):
        self.prepare_baseline()
        self.write_deploy_output()
        _, headers, _ = ready_response()
        hostile = [
            {"phase": "durable_gateway", "class": "postgres://user:" + SENTINEL + "@db/app"},
            {"phase": "durable_gateway", "class": SENTINEL},
            {"phase": "durable_gateway", "class": "Checkpoint_Load_Failed"},
            {"phase": "durable_gateway", "class": "a" * 33},
            {"phase": "", "class": "checkpoint_load_failed"},
            {"phase": "durable gateway", "class": "checkpoint_load_failed"},
            {"phase": "durable_gateway", "class": "checkpoint_load_failed\n"},
            {"phase": "durable_gateway", "class": 7},
            {"phase": "durable_gateway"},
            "durable_gateway:checkpoint_load_failed", None, ["durable_gateway", "x"],
        ]
        for value in hostile:
            with self.subTest(value=value):
                self.clock.now = 100
                client = verify_client()
                body = self.readyz_body(None, gateway_failure=value)
                client.request_routes[URL + "/readyz"] = [(503, headers, body)]
                self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
                self.assertEqual(
                    client.observation,
                    "rollout=converged readyz=503 components=process:ready,gateway:down identity=match")
                self.assertNotIn(SENTINEL, client.observation)
        self.assert_no_secret_saved_or_printed()

    def test_failure_with_extra_keys_prints_only_the_two_tokens(self):
        self.assertEqual(
            rollout.runtime_observation(
                503, {"x-two-worker-version": VERSION},
                self.readyz_body({"phase": "durable_gateway", "class": "gateway_runtime_failed",
                                  "error": SENTINEL}),
                VERSION, REVISION, BUILD_ID),
            "readyz=503 components=process:ready,gateway:down identity=match "
            "gateway_failure=durable_gateway:gateway_runtime_failed")

    def unconverged_client(self, readyz):
        pending = completed_row()
        pending["status"] = "progressing"
        pending["health"] = {"instances": {"active": 0, "healthy": 0, "failed": 0, "starting": 1,
                                           "scheduling": 0}}
        client = verify_client()
        client.api_routes[DETAIL_PATH] = [pending]
        client.request_routes[URL + "/readyz"] = [readyz]
        return client

    def test_unconverged_rollout_still_surfaces_the_gateway_failure_of_this_build(self):
        self.prepare_baseline()
        self.write_deploy_output()
        failure = {"phase": "durable_gateway", "class": "automod_config_invalid"}
        self.clock.now = 100
        client = self.unconverged_client(
            (503, {"x-two-worker-version": VERSION}, self.readyz_body(failure)))
        self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
        self.assertEqual(
            client.observation,
            "rollout=progressing instances=active:0,healthy:0,failed:0,starting:1,scheduling:0 "
            "gateway_failure=durable_gateway:automod_config_invalid")
        self.assert_no_evidence()

    def test_unconverged_rollout_ignores_a_failure_from_another_worker_or_image(self):
        self.prepare_baseline()
        self.write_deploy_output()
        failure = {"phase": "durable_gateway", "class": "automod_config_invalid"}
        stale = json.loads(self.readyz_body(failure))
        stale["build_id"] = "999999-1"
        for headers, body in [({"x-two-worker-version": OLD_VERSION}, self.readyz_body(failure)),
                              ({}, self.readyz_body(failure)),
                              ({"x-two-worker-version": VERSION}, json.dumps(stale).encode()),
                              ({"x-two-worker-version": VERSION}, SENTINEL.encode()),
                              ({"x-two-worker-version": VERSION}, b"[]"),
                              ({}, b"")]:
            with self.subTest(headers=headers):
                self.clock.now = 100
                client = self.unconverged_client((503, headers, body))
                self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
                self.assertEqual(client.observation,
                                 "rollout=progressing instances=active:0,healthy:0,failed:0,starting:1,scheduling:0")
        self.assert_no_secret_saved_or_printed()

    def test_main_prints_the_gateway_failure_in_the_last_observation(self):
        self.prepare_baseline()
        self.write_deploy_output()
        argv = ["staging_rollout.py", "verify", "--receipt", self.args.receipt,
                "--output", self.args.output, "--evidence", self.args.evidence]
        _, headers, _ = ready_response()
        body = self.readyz_body({"phase": "durable_gateway", "class": "milestones_load_failed"})
        client = verify_client()
        client.request_routes[URL + "/readyz"] = [(503, headers, body)]
        self.stdout.seek(0)
        self.stdout.truncate()
        with patch.object(sys, "argv", argv), patch.object(rollout, "Client", return_value=client):
            self.assertEqual(rollout.main(), 1)
        self.assertEqual(self.stdout.getvalue().splitlines(), [
            "staging rollout gate failed: rollout_timeout",
            "last observation before timeout: rollout=converged readyz=503 "
            "components=process:ready,gateway:down identity=match "
            "gateway_failure=durable_gateway:milestones_load_failed"])

    def test_main_prints_last_observation_only_for_rollout_timeout(self):
        self.prepare_baseline()
        self.write_deploy_output()
        argv = ["staging_rollout.py", "verify", "--receipt", self.args.receipt,
                "--output", self.args.output, "--evidence", self.args.evidence]
        _, headers, _ = ready_response()
        body = json.dumps({"build_revision": REVISION, "build_id": BUILD_ID,
                           "components": [["process", "ready"], ["gateway", "down"]]}).encode()
        client = verify_client()
        client.request_routes[URL + "/readyz"] = [(503, headers, body)]
        self.stdout.seek(0)
        self.stdout.truncate()
        with patch.object(sys, "argv", argv), patch.object(rollout, "Client", return_value=client):
            self.assertEqual(rollout.main(), 1)
        self.assertEqual(self.stdout.getvalue().splitlines(), [
            "staging rollout gate failed: rollout_timeout",
            "last observation before timeout: rollout=converged readyz=503 "
            "components=process:ready,gateway:down identity=match"])
        self.stdout.seek(0)
        self.stdout.truncate()
        self.clock.now = 100
        client = verify_client()
        changed = app()
        changed["id"] = "different_application"
        client.api_routes[APP_PATH] = [[changed]]
        with patch.object(sys, "argv", argv), patch.object(rollout, "Client", return_value=client):
            self.assertEqual(rollout.main(), 1)
        self.assertEqual(self.stdout.getvalue().splitlines(),
                         ["staging rollout gate failed: application_identity_drift"])
        self.assert_no_secret_saved_or_printed()

    def test_health_status_or_serving_worker_mismatch_never_succeeds(self):
        self.prepare_baseline()
        self.write_deploy_output()
        for health in [(500, {"x-two-worker-version": VERSION}, SENTINEL.encode()),
                       (200, {"x-two-worker-version": OLD_VERSION}, SENTINEL.encode())]:
            with self.subTest(status=health[0], version=health[1]):
                self.clock.now = 100
                client = verify_client()
                client.request_routes[URL + "/health"] = [health]
                self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
                self.assert_no_evidence()
        self.assert_no_secret_saved_or_printed()

    def test_control_plane_identity_drift_never_saves_evidence(self):
        self.prepare_baseline()
        self.write_deploy_output()
        for key, value, code in [("id", "different_application", "application_identity_drift"),
                                 ("durable_objects", {"namespace_id": "different_namespace"},
                                  "application_identity_drift"),
                                 ("configuration", {"image": OLD_IMAGE}, "application_image_drift")]:
            with self.subTest(key=key):
                client = verify_client()
                client.deadline = 400  # long enough to exhaust the stale-image tolerance
                changed = app()
                changed[key] = value
                client.api_routes[APP_PATH] = [[changed]]
                self.assert_gate(code, rollout.verify, self.args, client)
                self.assert_no_evidence()

    def test_application_listing_trailing_the_completed_rollout_is_tolerated(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        stale = app()
        stale["configuration"] = {"image": OLD_IMAGE}
        client.api_routes[APP_PATH] = [[stale], [stale], [app()]]
        client.deadline = 400
        rollout.verify(self.args, client)
        evidence = json.loads(Path(self.args.evidence).read_text())
        self.assertEqual(evidence["image"], IMAGE)
        self.assertNotIn("active_lag", evidence)
        # Stale polls never count toward acceptance: two waits, then the fresh pass.
        self.assertEqual(self.clock.sleeps[:2], [5, 5])
        self.assert_no_secret_saved_or_printed()

    def test_stale_application_image_is_reported_and_never_accepted(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        stale = app()
        stale["configuration"] = {"image": OLD_IMAGE}
        client.api_routes[APP_PATH] = [[stale]]
        client.deadline = 100 + 5 * 3  # times out before the tolerance runs out
        self.assert_gate("rollout_timeout", rollout.verify, self.args, client)
        self.assertIn("application_image=stale", client.observation)
        self.assert_no_evidence()

    def test_persistent_stale_application_image_fails_as_drift_after_the_tolerance(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        stale = app()
        stale["configuration"] = {"image": OLD_IMAGE}
        client.api_routes[APP_PATH] = [[stale]]
        client.deadline = 1000
        self.assert_gate("application_image_drift", rollout.verify, self.args, client)
        self.assertEqual(client.calls.count(("api", APP_PATH)), rollout.APPLICATION_IMAGE_STALE_POLLS + 1)
        self.assert_no_evidence()

    def test_final_application_id_namespace_and_image_are_rechecked(self):
        self.prepare_baseline()
        self.write_deploy_output()
        for key, value in [("id", "different_application"),
                           ("durable_objects", {"namespace_id": "different_namespace"}),
                           ("configuration", {"image": OLD_IMAGE})]:
            with self.subTest(key=key):
                client = verify_client()
                changed = app()
                changed[key] = value
                client.api_routes[APP_PATH] = [[app()], [changed]]
                self.assert_gate("application_image_drift", rollout.verify, self.args, client)
                self.assertIn(("request", URL + "/readyz"), client.calls)
                self.assertEqual(client.calls.count(("api", APP_PATH)), 2)
                self.assert_no_evidence()
        self.assert_no_secret_saved_or_printed()

    def test_final_rollout_id_is_rechecked(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        changed = completed_row()
        changed["id"] = "different_final_rollout"
        client.api_routes[DETAIL_PATH] = [completed_row(), changed]
        self.assert_gate("rollout_identity_drift", rollout.verify, self.args, client)
        self.assertIn(("request", URL + "/readyz"), client.calls)
        self.assert_no_evidence()

    def test_worker_namespace_change_fails(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        changed = bindings()
        changed["resources"]["bindings"][0]["namespace_id"] = "d" * 64
        client.api_routes[NAMESPACE_PATH] = [changed]
        self.assert_gate("worker_namespace_changed", rollout.verify, self.args, client)
        self.assert_no_evidence()

    def test_pinned_detail_must_not_change_id_or_target_version(self):
        self.prepare_baseline()
        self.write_deploy_output()
        for key, value in [("id", "different_rollout"), ("target_version", 9)]:
            with self.subTest(key=key):
                client = verify_client()
                changed = completed_row()
                changed[key] = value
                client.api_routes[DETAIL_PATH] = [changed]
                self.assert_gate("rollout_identity_drift", rollout.verify, self.args, client)
                self.assert_no_evidence()

    def test_new_rollout_missing_target_version_fails(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        missing = completed_row()
        del missing["target_version"]
        client.api_routes[ROWS_PATH] = [[old_row(), missing]]
        self.assert_gate("invalid_api_schema", rollout.verify, self.args, client)
        self.assertNotIn(("request", URL + "/readyz"), client.calls)
        self.assert_no_evidence()

    def test_post_probe_replaced_rollout_and_worker_drift_fail(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        replaced = completed_row()
        replaced["status"] = "replaced"
        client.api_routes[DETAIL_PATH] = [completed_row(), replaced]
        self.assert_gate("rollout_replaced_or_reverted", rollout.verify, self.args, client)
        self.assert_no_evidence()
        client = verify_client()
        old = deployment()
        old["deployments"][0]["versions"][0]["version_id"] = OLD_VERSION
        client.api_routes[DEPLOYMENTS_PATH] = [deployment(), old]
        self.assert_gate("worker_version_not_active", rollout.verify, self.args, client)
        self.assert_no_evidence()

    def test_prepare_api_failure_is_not_success_and_creates_no_receipt(self):
        for path in [APP_PATH, ROWS_PATH]:
            with self.subTest(path=path):
                client = FakeClient({APP_PATH: [[app()]], ROWS_PATH: [[old_row()]]})
                client.api_routes[path] = [rollout.GateError("api_http_failure")]
                self.assert_gate("api_http_failure", rollout.prepare, self.args, client)
                self.assertFalse(Path(self.args.receipt).exists())
        self.assert_no_secret_saved_or_printed()

    def test_verify_api_failures_at_each_stage_never_become_success(self):
        self.prepare_baseline()
        self.write_deploy_output()
        for path in [NAMESPACE_PATH, APP_PATH, ROWS_PATH, DETAIL_PATH, DEPLOYMENTS_PATH]:
            with self.subTest(path=path):
                client = verify_client()
                client.api_routes[path] = [rollout.GateError("api_http_failure")]
                self.assert_gate("api_http_failure", rollout.verify, self.args, client)
                self.assert_no_evidence()
        # A failure of the final read must not be ignored after successful probes.
        client = verify_client()
        client.api_routes[DETAIL_PATH] = [completed_row(), rollout.GateError("api_transport_failure")]
        self.assert_gate("api_transport_failure", rollout.verify, self.args, client)
        self.assert_no_evidence()
        client = verify_client()
        client.api_routes[APP_PATH] = [[app()], rollout.GateError("api_http_failure")]
        self.assert_gate("api_http_failure", rollout.verify, self.args, client)
        self.assert_no_evidence()
        self.assert_no_secret_saved_or_printed()

    def test_receipt_mode_accepts_real_shape_without_cloudflare_credentials_or_network(self):
        self.prepare_baseline()
        self.write_deploy_output(real_shape_receipts())
        argv = ["staging_rollout.py", "receipt", "--receipt", self.args.receipt,
                "--output", self.args.output, "--evidence", self.args.evidence]
        with patch.dict(os.environ, {}, clear=True), patch.object(sys, "argv", argv), \
                patch.object(rollout, "Client", side_effect=AssertionError("receipt must be network-free")):
            self.assertEqual(rollout.main(), 0)
        self.assertEqual(json.loads(Path(self.args.evidence).read_text()), {"worker_version": VERSION})
        self.assertIn("wrangler deploy receipt accepted", self.stdout.getvalue())
        self.docker.assert_not_called()
        self.assert_no_secret_saved_or_printed()

    def test_receipt_mode_rejections_stop_before_ownership_moves(self):
        self.prepare_baseline()
        stale = real_shape_receipts()
        stale[2]["timestamp"] = date(-1)
        wrong = real_shape_receipts()
        wrong[0]["wrangler_version"] = "4.142.0"
        for records, code in [(stale, "stale_deploy_receipt"),
                              (wrong, "wrong_wrangler_receipt"),
                              (real_shape_receipts()[:2], "deploy_receipt_missing_or_ambiguous")]:
            with self.subTest(code=code):
                self.write_deploy_output(records)
                self.assert_gate(code, rollout.receipt, self.args)
                self.assert_no_evidence()

    def test_main_prints_allowlisted_diagnostic_after_gate_code(self):
        self.prepare_baseline()
        records = real_shape_receipts()
        records[0]["wrangler_version"] = "4.142.0"
        records[0]["command_line_args"] = [SENTINEL]
        self.write_deploy_output(records)
        argv = ["staging_rollout.py", "verify", "--receipt", self.args.receipt,
                "--output", self.args.output, "--evidence", self.args.evidence]
        with patch.object(sys, "argv", argv), patch.object(rollout, "Client", return_value=verify_client()):
            self.assertEqual(rollout.main(), 1)
        self.assertEqual(self.stdout.getvalue().splitlines()[1:], [
            "staging rollout gate failed: wrong_wrangler_receipt",
            "staging rollout diagnostic: records deploy=1,wrangler-session=2; "
            "sessions deploy=1,probe=0,other=1; v1/wrangler-4.142.0 v1/wrangler-4.147.0",
        ])
        self.assert_no_evidence()
        # The caller-owned NDJSON input legitimately holds the sentinel; only
        # printed output and generated files are checked.
        self.assertNotIn(SENTINEL, self.stdout.getvalue() + self.stderr.getvalue())
        for path in [self.args.receipt, self.args.deploy_config]:
            self.assertNotIn(SENTINEL, Path(path).read_text())

    def test_main_hides_raw_unexpected_exception_and_returns_failure(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        client.api_routes[APP_PATH] = [RuntimeError(SENTINEL)]
        argv = ["staging_rollout.py", "verify", "--receipt", self.args.receipt,
                "--output", self.args.output, "--evidence", self.args.evidence]
        with patch.object(sys, "argv", argv), patch.object(rollout, "Client", return_value=client):
            self.assertEqual(rollout.main(), 1)
        self.assertIn("staging rollout gate failed: invalid_local_receipt", self.stdout.getvalue())
        self.assert_no_evidence()
        self.assert_no_secret_saved_or_printed()

    def test_main_reports_sanitized_api_failure_without_raw_error_or_traceback(self):
        self.prepare_baseline()
        self.write_deploy_output()
        client = verify_client()
        client.api_routes[APP_PATH] = [rollout.GateError("api_failure")]
        argv = ["staging_rollout.py", "verify", "--receipt", self.args.receipt,
                "--output", self.args.output, "--evidence", self.args.evidence]
        with patch.object(sys, "argv", argv), patch.object(rollout, "Client", return_value=client):
            self.assertEqual(rollout.main(), 1)
        self.assertIn("staging rollout gate failed: api_failure", self.stdout.getvalue())
        self.assertEqual(self.stderr.getvalue(), "")
        self.assert_no_evidence()
        self.assert_no_secret_saved_or_printed()

    def test_invalid_ndjson_is_sanitized_by_main(self):
        self.prepare_baseline()
        Path(self.args.output).write_text(SENTINEL)
        argv = ["staging_rollout.py", "verify", "--receipt", self.args.receipt,
                "--output", self.args.output, "--evidence", self.args.evidence]
        with patch.object(sys, "argv", argv), patch.object(rollout, "Client", return_value=verify_client()):
            self.assertEqual(rollout.main(), 1)
        self.assertIn("staging rollout gate failed: invalid_json", self.stdout.getvalue())
        self.assertNotIn(SENTINEL, self.stdout.getvalue() + self.stderr.getvalue())
        # The raw input remains caller-owned; no generated file may copy it.
        self.assertFalse(Path(self.args.evidence).exists())
        for path in [self.args.receipt, self.args.deploy_config]:
            self.assertNotIn(SENTINEL, Path(path).read_text())

    def test_verify_rejects_non_staging_or_credentialed_url(self):
        self.prepare_baseline()
        self.write_deploy_output()
        for url in ["https://two-bot-next.offline-fixture.workers.dev", "http://" + URL[8:],
                    "https://user:password@" + URL[8:], URL + "/other"]:
            with self.subTest(url=url):
                with patch.dict(os.environ, {"STAGING_URL": url}):
                    self.assert_gate("invalid_staging_url", rollout.verify, self.args, verify_client())
                self.assert_no_evidence()


class UserAgentTests(unittest.TestCase):
    def test_requests_send_an_explicit_user_agent(self):
        seen = []

        class Response(io.BytesIO):
            status = 200
            headers = {}

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        class Opener:
            def open(self, request, timeout):
                seen.append(request.get_header("User-agent"))
                return Response(b"ok")

        client = rollout.Client("0" * 32, "token")
        client.opener = Opener()
        client.request("https://example.invalid/readyz")
        self.assertEqual(seen, [rollout.USER_AGENT])
        self.assertFalse(seen[0].startswith("Python-urllib"))


if __name__ == "__main__":
    unittest.main()
