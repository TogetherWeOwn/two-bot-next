#!/usr/bin/env python3
"""Offline nightly acceptance; --real-cargo opts into the bounded-pool regression."""

import ast
from collections import Counter
import io
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import yaml


ROOT = Path(__file__).resolve().parents[2]
RUNNER = ROOT / "scripts/ci/run-nightly-partition.py"
PARTITIONS = range(4)
CARGO_FLAGS = ("--workspace", "--all-features", "--locked", "--no-fail-fast")

# Original filtered inventory contract, captured from main at 76b048e3b39c.
ORIGINAL_SKIPS = (
    'regenerate_reference_docs',
    'channel_moderation_store::',
    'channel_moderation_store::tests::lockdown_round_trip_stores_and_restores_exact_masks',
    'channel_moderation_store::tests::repeated_lockdown_preserves_first_masks',
    'channel_moderation_store::tests::live_unlock_refreshes_the_next_lockdown_seed_and_generation',
    'channel_moderation_store::tests::delayed_unlock_cleanup_cannot_delete_a_later_lockdown',
    'channel_moderation_store::tests::retried_cleanup_after_recreation_reports_stale',
    'channel_moderation_store::tests::claim_complete_replay_refuses_reuse',
    'channel_moderation_store::tests::release_returns_key_for_a_real_retry',
    'channel_moderation_store::tests::stale_claim_writes_cannot_affect_a_new_winner',
    'channel_moderation_store::tests::action_only_reuse_is_mismatch_in_flight_and_done',
    'channel_moderation_store::tests::audit_rows_are_written_once',
    'channel_moderation_store::tests::shared_timestamp_schema_preserves_channel_claim_and_audit_instants',
    'channel_moderation_store::tests::channel_lane_blocks_distinct_keys_until_safe_release_or_finish',
    'channel_moderation_store::tests::finish_rolls_back_completion_and_lane_release_when_audit_fails',
    'channel_moderation_store::tests::finish_restoration_cleanup_is_atomic_and_generation_fenced',
    'channel_moderation_store::tests::stale_finish_cannot_audit_or_release_a_new_claim_generation',
    'channel_moderation_store::tests::shared_ledger_accepts_channel_and_member_timestamp_bindings',
    'channel_moderation_store::tests::timestamp_upgrade_preserves_legacy_rows_and_claim_generations',
    'defer_precedes_sql_and_effect_then_edits_original_with_no_mentions',
    'rejected_or_ambiguous_defer_never_claims_or_mutates',
    'failed_result_edit_never_repeats_a_completed_effect',
    'persistence_failure_edits_safe_reconciliation_reply_and_keeps_lane',
    'disabled_command_uses_ephemeral_lifecycle_and_other_slices_are_untouched',
    'shared_router_gates_bounds_and_required_reason_refuse_without_http',
    'purge_and_slowmode_success_audit_and_replay_without_repeating_effects',
    'purge_of_messages_older_than_fourteen_days_deletes_them_singly',
    'purge_stopped_early_reports_the_reason_in_the_reply_and_the_audit_row',
    'full_purge_reply_and_audit_metadata_are_byte_identical_to_before',
    'repeated_lockdown_keeps_first_seed_and_unlock_restores_exact_masks',
    'initially_absent_overwrite_is_deleted_only_after_recorded_lockdown',
    'unlock_preserves_access_restrictions_added_after_lockdown',
    'unlock_drift_and_read_failure_refuse_without_mutation_or_recovery_loss',
    'lockdown_after_an_external_unlock_refreshes_seed_and_fences_old_cleanup',
    'ambiguous_unlock_retains_recovery_and_both_claims_across_all_keys',
    'unlock_proceeds_after_purge_history_unavailable',
    'unlock_proceeds_after_purge_history_rate_limited',
    'unlock_proceeds_after_purge_history_unreadable',
    'unlock_proceeds_after_purge_history_timeout',
    'purge_delete_failure_still_retains_the_lane',
    'rate_limited_mutation_proves_no_effect_and_frees_the_lane',
    'rejected_first_lock_retires_only_its_seed_and_finishes_refusal',
    'audit_failure_retries_only_finalization_not_a_successful_discord_effect',
    'first_seed_and_durable_lane_exist_before_lock_put_is_accepted',
    'exhausted_audit_failure_retains_lane_and_cannot_redo_successful_effect',
    'stale_tickets_and_action_only_mismatch_never_admit_runtime_effects',
    'delayed_unlock_cannot_overwrite_a_later_lockdown_cycle',
    'post_mutation_audit_failure_rolls_back_completion_and_retains_recovery',
    'rejected_repeated_lockdown_preserves_original_seed_and_unlock_failure_keeps_it',
    'unlock_preserves_live_privacy_changes_instead_of_restoring_a_snapshot',
    'unlock_send_bit_drift_aborts_without_consuming_the_recovery_seed',
    'stale_channel_ticket_and_recovery_generation_cannot_finish_current_owner',
    'ambiguous_permission_responses_retain_request_channel_and_recovery_fences',
    'recovery_read_failures_release_both_fences_and_preserve_original_seed',
    'rejected_channel_reservations_release_request_claim_and_allow_same_key_retry',
    'unknown_channel_reservation_completion_retains_request_claim',
    'rejected_recovery_writes_release_reservations_without_discord_mutation',
    'purge_history_failures_are_retryable_without_any_deletion',
    'uncertain_purge_deletions_keep_same_key_and_distinct_key_fenced',
    'purge_counts_bulk_delete_and_replays_without_rest_or_duplicate_audit',
    'purge_stopped_early_reports_the_reason_in_the_result_and_the_audit_row',
    'full_purge_audit_metadata_and_stored_payload_are_byte_identical_to_before',
    'purge_corrupt_stop_reason_replay_fails_closed_without_discord_requests',
    'slowmode_zero_and_signed_maximum_unicode_reason_survive_replay',
    'lockdown_and_unlock_restore_full_masks_and_replay_independently',
    'missing_overwrite_is_deleted_on_unlock_and_untracked_unlock_refuses',
    'concurrent_lock_and_unlock_are_ordered_across_independent_pools',
    'uncertain_lockdown_keeps_seed_and_channel_fence_against_distinct_key_unlock',
    'rejected_lockdown_removes_only_new_seed_and_allows_same_key_retry',
    'permission_actor_and_guild_refusals_never_mutate',
    'single_delete_429_releases_attempt_for_same_key_retry',
    'bulk_delete_429_releases_attempt_for_same_key_retry',
    'channel_overwrite_429_releases_attempt_and_retires_only_its_seed',
    'slowmode_429_releases_attempt_for_same_and_distinct_keys',
    'unlock_429_releases_lane_but_preserves_recovery_seed',
    'lockdown_429_preserves_prior_seed_while_releasing_the_lane',
    'slowmode_5xx_and_unexpected_statuses_stay_fenced',
    'slowmode_timeout_stays_fenced',
    'cancelled_lockdown_stays_fenced_with_seed_retained',
    'feed_crud_audit_scope_and_timestamp_roundtrip',
    'claim_is_owned_once_then_in_flight',
    'failed_send_releases_the_claim_for_the_retry',
    'delivered_rows_are_never_reclaimed',
    'delivered_rows_expire_only_past_the_horizon_with_the_fence_intact',
    'fresh_claim_cannot_be_reclaimed',
    'stale_unposted_claim_is_taken_over_atomically',
    'started_claim_is_never_reclaimed_even_when_stale',
    'started_send_that_provably_failed_releases_for_the_retry',
    'reclaiming_a_missing_key_claims_it_fresh',
    'delivered_rows_expire_only_past_the_horizon_with_the_fence_intact',
    'concurrent_claims_lease_takeover_and_stale_owner_fencing',
    'mock_delivery_crash_reconciliation_restart_and_overflow',
    'parity_mismatch_cursor_and_read_only_acceptance',
    'legacy_verify_policy_gates_the_real_test_endpoints',
    'dump_inspect_restore_round_trip',
    'flags_are_guild_scoped_half_open_deduplicated_and_activity_protected',
    'list_is_guild_scoped_selector_shaped_and_read_only',
    'seeded_aggregates_render_exactly_guild_scoped_and_read_only',
    'real_sqlx_runner_cases',
    'post_failure_unreadable_ledger_is_unknown_not_empty',
    'post_failure_readable_ledger_keeps_array_output',
    'post_apply_unreadable_ledger_is_unknown_not_empty',
    'store_failure_keeps_committed_cutover_evidence',
    'connection_count_sees_other_roles_from_plan_login',
    'database::actions_persist_rows_and_preserve_other_events_and_guilds',
    'blocklisted_members_skip_per_user_rows_but_count_in_aggregates',
    'persisted_member_rows_hold_ids_timestamps_enums_only',
    'resume_continues_from_the_persisted_cursor',
    'backfill_cursors_persist_per_family_insert_and_update',
    'backfill_cursor_survives_crash_restart_and_never_resets_to_zero',
    'concurrent_families_never_clobber_each_other',
    'backfill_cursor_rows_hold_ids_cursors_timestamps_only',
    'history_pagination_honors_limit_range_and_before_after_around_cursors',
    'history_reads_all_pass_through_rate_limit_guard',
    'history_rows_hold_ids_timestamps_counts_only_with_body_tripwire',
    'history_resume_continues_from_persisted_cursor_after_restart',
    'audit_log_pagination_limit_1_to_100_before_after_covers_45d_window',
    'audit_log_requests_ride_rate_limit_client_never_direct_rest',
    'audit_cursor_persists_per_window_and_resumes_mid_window',
    'audit_rows_hold_ids_timestamps_enums_only',
    'audit_429_retry_after_pauses_without_losing_place',
    'event_users_paginate_through_rate_limit_client_with_max_100',
    'reaction_users_paginate_through_rate_limit_client',
    'pins_list_through_rate_limit_client_persists_metadata_only',
    'blocklisted_users_skipped_for_per_user_rows_while_aggregates_keep_counting',
    'persisted_rows_hold_ids_timestamps_enums_only',
    'resume_continues_from_persisted_cursors_per_family',
    'bans_fetch_applies_blocklist_and_counts_everyone',
    'invite_rows_hold_metadata_only',
    'onboarding_rows_hold_ids_enums_metadata_only',
    'bans_invites_onboarding_resume_continues_from_persisted_cursors',
    'event_store_s8_stage_lifecycle_persists_ids_and_privacy_enum_only',
    'event_store_s8_interaction_create_observes_ids_and_command_enums_only',
    'event_store_s8_audit_log_entry_persists_ids_and_action_enum_only',
    'event_store_s8_duplicate_delivery_inserts_once',
    'event_store_s8_privacy_tripwire_rejects_topic_token_and_change_details',
    'event_store_s2_member_add_persists_ids_and_premium_since_only',
    'event_store_s2_member_remove_persists_ids_only',
    'event_store_s2_member_update_persists_nick_and_role_deltas_only',
    'event_store_s2_duplicate_delivery_inserts_once',
    'event_store_s2_privacy_tripwire_rejects_bio_text',
    'event_store_s4_voice_state_join_persists_ids_session_and_flags_only',
    'event_store_s4_voice_server_update_persists_ids_without_token_or_endpoint',
    'event_store_s4_duplicate_delivery_inserts_once',
    'event_store_s4_privacy_tripwire_rejects_effect_and_status_text',
    'event_store_s5_message_update_persists_ids_without_body',
    'event_store_s5_message_delete_persists_ids_only',
    'event_store_s5_reaction_add_remove_persist_emoji_refs_only',
    'event_store_s5_bulk_delete_fans_out_one_row_per_id',
    'event_store_s5_unicode_reaction_stores_enum_ref_only',
    'event_store_s5_privacy_tripwire_rejects_message_bodies',
    'event_store_s6_thread_create_persists_ids_and_flag_enums_only',
    'event_store_s6_thread_update_persists_changed_flags_only',
    'event_store_s6_thread_delete_persists_ids_only',
    'event_store_s6_list_sync_fans_out_one_row_per_thread',
    'event_store_s6_member_update_remove_persist_member_id_deltas_only',
    'event_store_s6_duplicate_delivery_inserts_once',
    'event_store_s6_privacy_tripwire_rejects_thread_names_and_bodies',
    'event_store_s7_invite_create_persists_code_hash_and_ids_only',
    'event_store_s7_invite_delete_persists_code_hash_and_ids_only',
    'event_store_s7_scheduled_event_lifecycle_persists_ids_and_status_enums_only',
    'event_store_s7_scheduled_event_user_add_remove_persist_user_id_deltas_only',
    'event_store_s7_duplicate_delivery_inserts_once',
    'event_store_s7_privacy_tripwire_rejects_descriptions_and_locations',
)

# Model Cargo/libtest's public CLI, independently of the partition algorithm.
# The ledger records the tests actually selected, not just the requested argv.
FAKE_CARGO = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

fixture = json.loads(Path(os.environ["NIGHTLY_FIXTURE"]).read_text())
args = sys.argv[1:]
log = Path(os.environ["NIGHTLY_LEDGER"])

def record(event):
    with log.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(event, ensure_ascii=False) + "\n")

record({"event": "command", "args": args})
if not args or args[0] not in ("test", "build"):
    print("unexpected Cargo command: " + repr(args), file=sys.stderr)
    sys.exit(97)

cargo, harness = args[1:], []
if "--" in cargo:
    index = cargo.index("--")
    cargo, harness = cargo[:index], cargo[index + 1:]

is_doc = "--doc" in cargo
is_list = "--list" in harness
is_compile = args[0] == "build" or "--no-run" in cargo
phase = "compile" if is_compile else "list" if is_list else "doc" if is_doc else "test"
# Listing a test target compiles it too, even without a separate --no-run call.
if fixture.get("fail_phase") == phase or fixture.get("fail_compile"):
    print("fixture failure in " + ("compile" if fixture.get("fail_compile") else phase), file=sys.stderr)
    sys.exit(23)
filters, skips = [], []
index = 0
while index < len(harness):
    token = harness[index]
    if token in ("--skip", "--test-threads", "--format", "--color"):
        if index + 1 >= len(harness):
            sys.exit(96)
        if token == "--skip":
            skips.append(harness[index + 1])
        index += 2
    elif token.startswith("--skip="):
        skips.append(token.split("=", 1)[1])
        index += 1
    elif token.startswith("-"):
        index += 1
    else:
        filters.append(token)
        index += 1

exact_filters = set(filters) if "--exact" in harness else None

def selected(test, *, listing=False):
    name = test["name"]
    if any(name == skip if exact_filters is not None else skip in name for skip in skips):
        return False
    if filters and (name not in exact_filters if exact_filters is not None else not any(item in name for item in filters)):
        return False
    if "--ignored" in harness and not test.get("ignored"):
        return False
    return listing or not test.get("ignored") or "--include-ignored" in harness or "--ignored" in harness

# Cargo's --tests selects every manifest test=true target, including examples.
# Default examples have test=false: default `cargo test` compiles them normally
# but --examples/--all-targets override that and build them with test harnesses.
selectors = {"--lib": "lib", "--bins": "bin", "--examples": "example"}
singular = {"--bin": "bin", "--test": "integration", "--example": "example"}
explicit = any(flag in cargo for flag in (*selectors, *singular, "--tests", "--all-targets"))

def manifest_test(target):
    return target.get("test", target["kind"] != "example")

def chosen(target):
    if not explicit:
        return target["kind"] in ("lib", "bin") if args[0] == "build" else manifest_test(target)
    if "--all-targets" in cargo or "--tests" in cargo and manifest_test(target):
        return True
    if any(flag in cargo and target["kind"] == kind for flag, kind in selectors.items()):
        return True
    return any(flag in cargo and target["kind"] == kind and
               cargo[cargo.index(flag) + 1] == target.get("name", target["id"].split(":")[-1])
               for flag, kind in singular.items())

targets = fixture["doctests"] if is_doc else [target for target in fixture["targets"] if chosen(target)]
normal_examples = [] if is_doc or explicit or args[0] != "test" else [
    target for target in fixture["targets"] if target["kind"] == "example" and not manifest_test(target)
]
for target in normal_examples + ([] if is_doc else targets):
    harness_build = args[0] == "test" and target not in normal_examples
    record({"event": "compile", "target": target["id"], "harness": harness_build})
    if not harness_build and target.get("normal_compile_error"):
        print(target["normal_compile_error"] + ": ordinary example build failed", file=sys.stderr)
        sys.exit(101)
if is_compile:
    sys.exit(0)
if not is_doc and not explicit:
    targets = targets + fixture["doctests"]
if fixture.get("reverse_inventory"):
    targets = list(reversed(targets))

if is_list:
    if "listing_override" in fixture:
        print(fixture["listing_override"])
    else:
        for target in targets:
            tests = [test for test in target["tests"] if selected(test, listing=True)]
            if fixture.get("reverse_inventory"):
                tests.reverse()
            for test in tests:
                print(test["name"] + ": test")
            print("\n%d tests, 0 benchmarks" % len(tests))
    sys.exit(0)

failed = False
if fixture.get("fail_first_batch"):
    with log.open(encoding="utf-8") as stream:
        failed = not any(json.loads(line)["event"] == "batch" for line in stream)
record({"event": "batch", "args": args, "filters": filters, "doc": is_doc})
for target in targets:
    for test in target["tests"]:
        if selected(test):
            event = {"event": "execute", "target": target["id"], "name": test["name"], "doc": target["kind"] == "doc"}
            if event["doc"]:
                event["args"] = args
            record(event)
            failed |= test["name"] in fixture.get("failing_tests", [])
if failed:
    print("fixture test batch failed", file=sys.stderr)
sys.exit(41 if failed else 0)
'''


def fixture():
    """Hand-built inventory; shared names belong to distinct Cargo targets."""
    return {
        "targets": [
            {
                "id": "alpha:lib",
                "kind": "lib",
                "tests": [
                    {"name": "suite::case"},
                    {"name": "suite::case_extended"},
                    {"name": "suite::shared"},
                    {"name": "suite::ignored", "ignored": True},
                    {"name": "suite::café"},
                    {"name": "suite::東京"},
                    *({"name": f"suite::normal_{index:03d}"} for index in range(48)),
                ],
            },
            {"id": "alpha:bin", "kind": "bin", "tests": [{"name": "binary::smoke"}, {"name": "suite::shared"}]},
            {"id": "beta:integration", "kind": "integration", "tests": [
                {"name": "integration::smoke"}, {"name": "suite::shared"},
                {"name": "property_no_operation_sequence_creates_a_cycle"},
            ]},
            {"id": "beta:example", "kind": "example", "test": True, "tests": [{"name": "example::smoke"}]},
        ],
        "doctests": [{"id": "alpha:doc", "kind": "doc", "tests": [{"name": "src/lib.rs - usage (line 12)"}]}],
    }


def example_fixture():
    data = fixture()
    data["targets"] += [
        {"id": "beta:normal-example", "kind": "example", "tests": [{"name": "ordinary_example::must_not_run"}]},
        {"id": "beta:disabled-example", "kind": "example", "test": False,
         "tests": [{"name": "disabled_example::must_not_run"}]},
    ]
    return data


def large_fixture(*, size=None, width=256):
    # Exceed the real process argument limit in aggregate. Do not impose a
    # smaller, artificial stack/argv limit than the nightly environment uses.
    if size is None:
        size = max(4096, 5 * os.sysconf("SC_ARG_MAX") // (width + 14) + 64)
    data = fixture()
    data["targets"] = [{
        "id": "alpha:lib", "kind": "lib",
        "tests": [{"name": f"batch::{index:05d}_" + "x" * width} for index in range(size)],
    }]
    return data


class CargoFixture(unittest.TestCase):
    def run_partition(self, data, partition, *, count=4, hash_seed="17", extra=(),
                      cargo_name="cargo", shell_command=None):
        self.assertTrue(RUNNER.is_file(), "nightly partition runner missing: scripts/ci/run-nightly-partition.py")
        with tempfile.TemporaryDirectory(prefix="nightly-acceptance-") as directory:
            temporary = Path(directory)
            cargo = temporary / "cargo"
            cargo.write_text(FAKE_CARGO, encoding="utf-8")
            cargo.chmod(0o755)
            inventory = temporary / "fixture.json"
            inventory.write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
            ledger = temporary / "ledger.jsonl"
            env = {
                "PATH": str(temporary) + os.pathsep + os.environ["PATH"],
                "HOME": str(temporary),
                "LANG": "C.UTF-8",
                "PYTHONHASHSEED": hash_seed,
                "NIGHTLY_FIXTURE": str(inventory),
                "NIGHTLY_LEDGER": str(ledger),
            }
            command = [
                sys.executable, str(RUNNER), "--index", str(partition), "--count", str(count), "--",
                cargo_name, "test", *CARGO_FLAGS, "--", "--include-ignored", "--test-threads=1", *extra,
            ]
            if shell_command is not None:
                command = ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", shell_command]
            result = subprocess.run(
                command,
                cwd=ROOT, env=env, text=True, capture_output=True, timeout=30,
            )
            events = [json.loads(line) for line in ledger.read_text().splitlines()] if ledger.exists() else []
        return result, events

    def run_sweep(self, data, **options):
        outputs = {}
        for partition in PARTITIONS:
            result, events = self.run_partition(data, partition, **options)
            self.assertEqual(result.returncode, 0, f"partition {partition}: {result.stdout}\n{result.stderr}")
            outputs[partition] = events
        return outputs

    @staticmethod
    def executions(events, *, doc=False):
        return Counter((event["target"], event["name"]) for event in events
                       if event["event"] == "execute" and event["doc"] == doc)

    def assert_complete(self, data, outputs, *, skips=()):
        expected = Counter((target["id"], test["name"]) for target in data["targets"] for test in target["tests"]
                           if target.get("test", target["kind"] != "example")
                           and not any(skip in test["name"] for skip in skips))
        observed = Counter()
        for events in outputs.values():
            observed.update(self.executions(events))
        missing, unexpected = expected - observed, observed - expected
        self.assertFalse(missing, f"{sum(missing.values())} missing executions; examples: {list(missing.items())[:3]}")
        self.assertFalse(unexpected, f"{sum(unexpected.values())} unexpected/duplicate executions; examples: {list(unexpected.items())[:3]}")


class RunnerAcceptance(CargoFixture):
    def test_four_partitions_cover_every_target_and_name_exactly_once(self):
        data = fixture()
        outputs = self.run_sweep(data)
        for partition, events in outputs.items():
            self.assertTrue(self.executions(events), f"partition {partition} silently succeeded empty")
        self.assert_complete(data, outputs)

    def test_default_examples_are_compiled_normally_without_becoming_tests(self):
        data = example_fixture()
        result, events = self.run_partition(data, 0, count=1)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        normal_builds = Counter(event["target"] for event in events
                                if event["event"] == "compile" and not event["harness"])
        for target in ("beta:normal-example", "beta:disabled-example"):
            with self.subTest(target=target):
                self.assertGreater(normal_builds[target], 0, "default example executable compilation was lost")
                self.assertFalse(any(key[0] == target for key in self.executions(events)),
                                 "ordinary examples must not be forced into --test execution")
        self.assert_complete(data, {0: events})

    def test_manifest_test_true_example_stays_eligible(self):
        outputs = self.run_sweep(fixture())
        self.assertEqual(sum(self.executions(events)[("beta:example", "example::smoke")]
                             for events in outputs.values()), 1,
                         "excluding ordinary examples must not lose manifest test=true example tests")

    def test_normal_example_build_failure_is_not_hidden_by_test_harness(self):
        data = example_fixture()
        data["targets"][-2]["normal_compile_error"] = "E0580"
        result, events = self.run_partition(data, 0, count=1)
        self.assertNotEqual(result.returncode, 0, "--test hid an invalid ordinary example main signature")
        self.assertIn("E0580", result.stdout + result.stderr)
        self.assertFalse(any(key[0] == "beta:normal-example" for key in self.executions(events)))

    def test_partition_ownership_is_deterministic_across_order_and_process_hash_seed(self):
        data = fixture()
        first = self.run_sweep(data, hash_seed="17")
        data["reverse_inventory"] = True
        second = self.run_sweep(data, hash_seed="431")
        for partition in PARTITIONS:
            self.assertEqual(self.executions(first[partition]), self.executions(second[partition]))

    def test_inventory_added_after_the_first_run_is_not_lost(self):
        data = fixture()
        self.run_sweep(data)
        data["targets"][0]["tests"].append({"name": "suite::added_later", "ignored": True})
        outputs = self.run_sweep(data)
        self.assert_complete(data, outputs)
        self.assertEqual(sum(self.executions(events)[("alpha:lib", "suite::added_later")]
                             for events in outputs.values()), 1)

    def test_positive_selection_is_exact_and_preserves_workspace_feature_graph(self):
        outputs = self.run_sweep(fixture())
        for events in outputs.values():
            for event in events:
                if event["event"] == "command":
                    args = event["args"]
                    cargo = args[:args.index("--")] if "--" in args else args
                    for flag in CARGO_FLAGS:
                        if flag != "--no-fail-fast" or cargo[0] == "test":
                            self.assertIn(flag, cargo)
                    self.assertFalse(set(cargo) & {"-p", "--package", "--exclude"}, "per-package invocations change the feature graph")
                elif event["event"] == "batch":
                    args = event["args"]
                    self.assertIn("--include-ignored", args)
                    self.assertTrue("--test-threads=1" in args or any(
                        args[index:index + 2] == ["--test-threads", "1"] for index in range(len(args))))
                    if not event["doc"]:
                        self.assertIn("--exact", args)
                        self.assertTrue(event["filters"], "a positive partition may not fall back to the full sweep")

    def test_substring_skips_are_applied_before_exact_positive_selection(self):
        data = fixture()
        data["targets"][0]["tests"] += [
            {"name": "guarded::mutation", "ignored": True},
            {"name": "prefix_guarded::mutation_suffix"},
            {"name": "guarded::delete"},
        ]
        skips = ("guarded::mutation", "guarded::delete")
        outputs = self.run_sweep(data, extra=("--skip", skips[0], "--skip", skips[1]))
        self.assert_complete(data, outputs, skips=skips)
        for events in outputs.values():
            for event in events:
                if event["event"] == "batch" and not event["doc"]:
                    self.assertFalse(any(skip in name for skip in skips for name in event["filters"]))

    def test_discovery_substring_skip_excludes_new_qualified_guarded_names(self):
        data = fixture()
        data["targets"][0]["tests"] += [
            {"name": "channel_moderation_store::new_guarded_case", "ignored": True},
            {"name": "wrapper::channel_moderation_store::new_guarded_case::later", "ignored": True},
            {"name": "channel_moderation_store_like::ordinary_case"},
            {"name": "channel_moderation_store_like::ordinary_case_extended"},
        ]
        outputs = self.run_sweep(data, extra=("--skip", "channel_moderation_store::"))
        self.assert_complete(data, outputs, skips=("channel_moderation_store::",))
        for events in outputs.values():
            for event in events:
                if event["event"] == "batch" and not event["doc"]:
                    self.assertFalse(any("channel_moderation_store::" in name for name in event["filters"]),
                                     "discovery must remove guarded substrings before --exact execution")

    def test_doctests_with_spaces_retain_their_original_selection_once(self):
        outputs = self.run_sweep(fixture())
        observed = Counter()
        for partition, events in outputs.items():
            selected = self.executions(events, doc=True)
            if partition:
                self.assertFalse(selected, "doctests must have one owner, partition 0")
            observed.update(selected)
            for event in events:
                if event["event"] == "execute" and event["doc"]:
                    self.assertNotIn("--exact", event["args"], "doctests retain the original non-exact selection")
        self.assertEqual(observed, Counter({("alpha:doc", "src/lib.rs - usage (line 12)"): 1}))

    def test_doctests_still_obey_original_skip_filters(self):
        outputs = self.run_sweep(fixture(), extra=("--skip", "usage"))
        self.assertFalse(any(self.executions(events, doc=True) for events in outputs.values()))

    def test_empty_or_malformed_inventory_is_not_a_green_sweep(self):
        for listing in ("", "0 tests, 0 benchmarks", "truncated inventory", "not_a_test: unknown", "\x00broken::name: test"):
            with self.subTest(listing=repr(listing)):
                data = fixture()
                data["listing_override"] = listing
                result, events = self.run_partition(data, 0)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse(self.executions(events))

    def test_partially_malformed_inventory_cannot_silently_omit_tests(self):
        data = fixture()
        data["listing_override"] = "suite::normal_000: test\nsuite::normal_001: te\n\n2 tests, 0 benchmarks"
        for partition in PARTITIONS:
            with self.subTest(partition=partition):
                result, events = self.run_partition(data, partition)
                self.assertNotEqual(result.returncode, 0, "a malformed row was silently dropped from the inventory")
                self.assertFalse(self.executions(events), "do not execute a partially accepted inventory")

    def test_empty_partition_is_rejected_without_falling_back_to_all_tests(self):
        data = fixture()
        data["targets"] = [{"id": "alpha:lib", "kind": "lib", "tests": [{"name": "only::one"}]}]
        data["doctests"] = []
        observed, succeeded = Counter(), 0
        for partition in PARTITIONS:
            result, events = self.run_partition(data, partition)
            selected = self.executions(events)
            if selected:
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                succeeded += 1
            else:
                self.assertNotEqual(result.returncode, 0, "empty partition silently accepted")
            observed.update(selected)
        self.assertEqual(succeeded, 1)
        self.assertEqual(observed, Counter({("alpha:lib", "only::one"): 1}))

    def test_invalid_partition_arguments_fail_before_any_tests_run(self):
        for index, count in ((-1, 4), (4, 4), (0, 0), (0, -1), ("wrong", 4), (0, "wrong")):
            with self.subTest(index=index, count=count):
                result, events = self.run_partition(fixture(), index, count=count)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.executions(events))

    def test_compilation_and_listing_failures_propagate(self):
        for options in ({"fail_compile": True}, {"fail_phase": "list"}):
            with self.subTest(options=options):
                data = fixture() | options
                result, events = self.run_partition(data, 0)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("fixture failure", result.stdout + result.stderr)
                self.assertFalse(self.executions(events))

    def test_missing_subprocess_is_not_a_success(self):
        result, events = self.run_partition(fixture(), 0, cargo_name="missing-nightly-cargo-command")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.executions(events))

    def test_doctest_failure_propagates(self):
        data = fixture()
        data["failing_tests"] = ["src/lib.rs - usage (line 12)"]
        result, events = self.run_partition(data, 0)
        self.assertTrue(self.executions(events, doc=True))
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_large_inventory_is_batched_without_omissions_or_duplicates(self):
        data = large_fixture()
        outputs = self.run_sweep(data)
        self.assert_complete(data, outputs)
        batches = [event for events in outputs.values() for event in events if event["event"] == "batch"]
        self.assertGreater(len(batches), 4, "inventory must be split into bounded argv batches")
        for batch in batches:
            self.assertLess(sum(len(os.fsencode(arg)) + 1 for arg in batch["args"]), os.sysconf("SC_ARG_MAX"))

    def test_failed_batch_does_not_mask_failure_or_prevent_later_batches(self):
        data = large_fixture() | {"fail_first_batch": True}
        outputs = {}
        for partition in PARTITIONS:
            result, events = self.run_partition(data, partition)
            self.assertNotEqual(result.returncode, 0, "a later successful batch masked the failure")
            self.assertIn("fixture test batch failed", result.stdout + result.stderr)
            self.assertGreater(sum(event["event"] == "batch" for event in events), 1)
            outputs[partition] = events
        self.assert_complete(data, outputs)

    def test_doctests_run_once_even_after_an_ordinary_batch_failure(self):
        data = large_fixture() | {
            "fail_first_batch": True,
            "failing_tests": ["src/lib.rs - usage (line 12)"],
        }
        outputs = {}
        for partition in PARTITIONS:
            result, events = self.run_partition(data, partition)
            self.assertNotEqual(result.returncode, 0, "a successful later batch masked earlier failures")
            ordinary_batches = [event for event in events if event["event"] == "batch" and not event["doc"]]
            self.assertGreater(len(ordinary_batches), 1, "later ordinary batches must run after the failed batch")
            doc_batches = [event for event in events if event["event"] == "batch" and event["doc"]]
            self.assertEqual(len(doc_batches), 1 if partition == 0 else 0,
                             "an ordinary failure must not suppress partition 0's doctest pass")
            expected_docs = Counter({("alpha:doc", "src/lib.rs - usage (line 12)"): 1}) if partition == 0 else Counter()
            self.assertEqual(self.executions(events, doc=True), expected_docs)
            self.assertIn("fixture test batch failed", result.stdout + result.stderr)
            outputs[partition] = events
        self.assert_complete(data, outputs)


def expression(value, context):
    """Evaluate only the small, side-effect-free Actions expression subset used here."""
    text = str(value).strip()
    if text.startswith("${{") and text.endswith("}}"):
        text = text[3:-2].strip()

    def lookup(path):
        def descend(current, parts):
            if not parts:
                return current
            if parts[0] == "*":
                values = current.values() if isinstance(current, dict) else current
                return [descend(item, parts[1:]) for item in values]
            return descend(current.get(parts[0], "") if isinstance(current, dict) else "", parts[1:])
        return descend(context, path.split("."))

    def substitute(match):
        token = match.group(0)
        return token if token.startswith(("'", '"')) else repr(lookup(token))

    text = re.sub(r"'[^']*'|\"[^\"]*\"|[A-Za-z_][\w-]*(?:\.(?:[\w-]+|\*))+", substitute, text)
    text = text.replace("&&", " and ").replace("||", " or ")
    text = re.sub(r"!(?!=)", "not ", text).strip()
    functions = {
        "always": lambda: True,
        "cancelled": lambda: context.get("_cancelled", False),
        "success": lambda: not context.get("_failed", False) and not context.get("_cancelled", False),
        "failure": lambda: context.get("_failed", False),
        "contains": lambda collection, item: item in collection,
        "toJSON": json.dumps,
        "fromJSON": json.loads,
    }
    tree = ast.parse(text, mode="eval")
    allowed = (ast.Expression, ast.BoolOp, ast.And, ast.Or, ast.UnaryOp, ast.Not,
               ast.Compare, ast.Eq, ast.NotEq, ast.Constant, ast.Call, ast.Name,
               ast.Load, ast.List, ast.Tuple, ast.Dict)
    for node in ast.walk(tree):
        if not isinstance(node, allowed):
            raise AssertionError(f"unsupported Actions expression: {value}")
        if isinstance(node, ast.Call) and (not isinstance(node.func, ast.Name) or node.func.id not in functions):
            raise AssertionError(f"unsupported Actions function: {value}")
    return eval(compile(tree, "<workflow expression>", "eval"), {"__builtins__": {}},
                functions | {"true": True, "false": False, "null": None} | context)


def condition(value, context):
    text = value or "success()"
    if not re.search(r"\b(always|success|failure|cancelled)\s*\(", str(text)):
        if not expression("success()", context):
            return False
    return bool(expression(text, context))


def render(value, context):
    def replace(match):
        result = expression(match.group(1), context)
        if isinstance(result, bool):
            return str(result).lower()
        if isinstance(result, (dict, list)):
            return json.dumps(result)
        return "" if result is None else str(result)
    return re.sub(r"\$\{\{(.*?)\}\}", replace, str(value), flags=re.DOTALL)


# Original replay interfaces, independently recorded before partitioning.
GUARDED_SUITES = (
    ("Channel moderation ignored tests with their guarded URL", "two-bot-core", "--lib", "channel_moderation_store::", False, True),
    ("Channel moderation runtime acceptance with its guarded URL", "two-bot-discord", "--test", "channel_moderation_runtime", False, True),
    ("Discord channel-moderation executor ignored tests with their guarded URL", "two-bot-discord", "--test", "internal_channel_moderation", False, True),
    ("Mutation-429 channel acceptance ignored tests with their guarded URL", "two-bot-discord", "--test", "mutation_429_proves_no_effect_channel", False, True),
    ("Feed store ignored tests with their guarded URL", "two-bot-core", "--test", "feeds_store", False, True),
    ("Legacy verification ignored tests with their guarded URL", "two-bot-cutover", "--test", "legacy_verify_db", False, True),
    ("Legacy TLS fence ignored tests with their guarded URL", "two-bot-cutover", "--test", "legacy_tls_fence", False, True),
    ("Reengagement list ignored tests with their guarded URL", "two-bot-cutover", "--test", "reengagement_list_db", True, True),
    ("Game-activity report tests with their guarded URL", "two-bot-cutover", "--test", "game_activity_report_db", True, False),
    ("Staging migration runner ignored tests with their guarded URL", "two-bot-cutover", "--test", "staging_migrate_db", False, True),
    ("Internal events ignored tests with their guarded URL", "two-bot-discord", "--test", "internal_events", True, True),
    ("Raid list ignored tests with their guarded URL", "two-bot-cutover", "--test", "raid_list_db", True, True),
)
BACKUP_STEPS = (
    "Build backup CLI",
    "Backup roundtrip on a dedicated disposable database",
    "Backup publication on a dedicated disposable database",
)
DOC_STEP = "cargo doc -D warnings"


class WorkflowAcceptance(CargoFixture):
    @classmethod
    def setUpClass(cls):
        cls.workflow = yaml.load((ROOT / ".github/workflows/nightly.yml").read_text(), Loader=yaml.BaseLoader)
        cls.sweep = cls.workflow["jobs"]["sweep"]

    def step(self, name):
        matches = [step for step in self.sweep["steps"] if step.get("name") == name]
        self.assertEqual(len(matches), 1, f"missing or duplicate step: {name}")
        return matches[0]

    def context(self, index=0, **overrides):
        matrix = self.sweep.get("strategy", {}).get("matrix", {})
        axes = [key for key in matrix if key not in ("include", "exclude")]
        self.assertEqual(len(axes), 1, "one four-way partition axis is required")
        context = {
            "matrix": {axes[0]: index},
            "needs": {"changes": {"result": "success", "outputs": {"rust": "true"}}, "sweep": {"result": "success"}},
            "steps": {"databases": {"outcome": "success"}, "backup-cli": {"outcome": "success"}},
            "github": {"workspace": str(ROOT)}, "runner": {"temp": "/tmp/nightly-fixture"},
            "_failed": False, "_cancelled": False,
        }
        return context | overrides

    def owners(self, step, **overrides):
        return [index for index in PARTITIONS if condition(step.get("if"), self.context(index, **overrides))]

    def test_matrix_has_exactly_four_legs_and_preserves_the_45_minute_limit(self):
        strategy = self.sweep.get("strategy", {})
        self.assertEqual(strategy.get("fail-fast"), "false")
        matrix = strategy.get("matrix", {})
        self.assertEqual(len(matrix), 1, "include/exclude must not add or remove partition legs")
        self.assertEqual(next(iter(matrix.values())), ["0", "1", "2", "3"])
        self.assertEqual(self.sweep["timeout-minutes"], "45")
        self.assertEqual(self.sweep.get("continue-on-error", "false"), "false")

    def test_rust_and_disposable_postgres_images_and_isolation_stay_pinned(self):
        self.assertEqual(self.sweep["container"]["image"],
                         "rust:1.98-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546")
        self.assertFalse(self.sweep["container"].get("volumes"))
        self.assertEqual(set(self.sweep["services"]), {"agent-testdb"})
        postgres = self.sweep["services"]["agent-testdb"]
        self.assertEqual(postgres["image"],
                         "postgres:18.6@sha256:5a5a84b19854a9ffaa54082c166ff4ec27473a361e496e5ea167f298f2da9722")
        self.assertEqual(postgres["env"], {"POSTGRES_USER": "agent_test", "POSTGRES_DB": "agent_test", "POSTGRES_HOST_AUTH_METHOD": "trust"})
        self.assertFalse(postgres.get("volumes"), "each matrix leg needs its own disposable database")

    def test_workflow_broad_command_preserves_the_complete_original_skip_contract(self):
        data = fixture()
        data["targets"][0]["tests"] += [
            {"name": prefix + name + suffix, "ignored": True}
            for name in ORIGINAL_SKIPS for prefix, suffix in (("", ""), ("wrapper::", "::later"))
        ]
        broad = self.step("Full workspace sweep including ignored tests")
        outputs = {}
        for index in PARTITIONS:
            self.assertTrue(condition(broad.get("if"), self.context(index)))
            result, events = self.run_partition(data, index, shell_command=render(broad["run"], self.context(index)))
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            outputs[index] = events
        self.assert_complete(data, outputs, skips=ORIGINAL_SKIPS)
        self.assertEqual(sum(self.executions(events)[("beta:integration", "property_no_operation_sequence_creates_a_cycle")]
                             for events in outputs.values()), 1, "an ordinary property test must not be newly skipped")
        for events in outputs.values():
            discoveries = [event for event in events if event["event"] == "command" and "--list" in event["args"]]
            self.assertTrue(discoveries, "the skip contract must be exercised by actual inventory commands")
            for event in discoveries:
                args = event["args"]
                harness = iter(args[args.index("--") + 1:])
                skips = []
                for argument in harness:
                    if argument == "--skip":
                        skips.append(next(harness))
                    elif argument.startswith("--skip="):
                        skips.append(argument.split("=", 1)[1])
                self.assertCountEqual(skips, ORIGINAL_SKIPS,
                                      "broad discovery may neither add nor remove original skip filters")
        for key, expected in {
            "TWO_TEST_DATABASE_URL": "postgres://agent_test:@agent-testdb:5432/two_bot_test_tog10090_nightly",
            "MEMBER_TESTDB": "agent-testdb", "TEST_DATABASE_URL": "postgresql://agent_test@agent-testdb:5432/postgres",
            "PAPERCLIP_RUN_SCRATCH_DIR": "${{ runner.temp }}/nightly-workspace",
        }.items():
            self.assertEqual(broad["env"][key], expected)
        self.assertNotIn("TWO_BOT_TEST_DATABASE_URL", self.sweep.get("env", {}) | broad["env"])

    def test_guarded_replays_keep_their_original_target_and_url_contract(self):
        for name, package, target_flag, target, empty_password, ignored in GUARDED_SUITES:
            with self.subTest(step=name):
                step = self.step(name)
                args = shlex.split(step["run"])
                self.assertIn(["-p", package], [args[index:index + 2] for index in range(len(args))])
                for flag in ("--all-features", "--locked", "--test-threads=1", target_flag, target):
                    self.assertIn(flag, args)
                self.assertEqual("--include-ignored" in args, ignored)
                self.assertNotIn("--skip", args)
                user = "agent_test:" if empty_password else "agent_test"
                self.assertEqual(step["env"]["TWO_TEST_DATABASE_URL"], f"postgres://{user}@agent-testdb:5432/agent_test")
        runtime = shlex.split(self.step(GUARDED_SUITES[1][0])["run"])
        self.assertEqual(runtime[:7], ["env", "-u", "PGPASSWORD", "-u", "PGPASSFILE", "-u", "PGOPTIONS"])

    def test_guarded_backup_and_rustdoc_steps_have_one_owner_and_keep_dependency_guards(self):
        for name in [row[0] for row in GUARDED_SUITES] + list(BACKUP_STEPS) + [DOC_STEP]:
            with self.subTest(step=name):
                step = self.step(name)
                self.assertEqual(self.owners(step), [0])
                self.assertEqual(self.owners(step, _failed=True), [0], "an earlier test failure must not suppress remaining results")
                self.assertEqual(self.owners(step, _cancelled=True), [])
                bad_database = {"databases": {"outcome": "failure"}, "backup-cli": {"outcome": "success"}}
                self.assertEqual(self.owners(step, steps=bad_database), [0] if name == DOC_STEP else [])
                self.assertEqual(step.get("continue-on-error", "false"), "false")
        publication = self.step(BACKUP_STEPS[2])
        bad_backup = {"databases": {"outcome": "success"}, "backup-cli": {"outcome": "failure"}}
        self.assertEqual(self.owners(publication, steps=bad_backup), [])

    def test_backups_and_strict_rustdoc_keep_their_original_interfaces(self):
        build, roundtrip, publication = [self.step(name) for name in BACKUP_STEPS]
        self.assertEqual(build["id"], "backup-cli")
        self.assertEqual(shlex.split(build["run"]), ["cargo", "build", "-p", "two-bot", "--bin", "two-bot", "--all-features", "--locked"])
        for step, target, database, scratch in (
            (roundtrip, "backup_roundtrip", "two_next_backup_nightly", "backup-roundtrip"),
            (publication, "backup_dump_publication", "two_next_backup_publication_nightly", "backup-publication"),
        ):
            with self.subTest(target=target):
                args = shlex.split(step["run"])
                self.assertIn(["--test", target], [args[index:index + 2] for index in range(len(args))])
                for flag in ("--all-features", "--locked", "--include-ignored", "--test-threads=1"):
                    self.assertIn(flag, args)
                self.assertEqual(step["env"]["TWO_BOT_TEST_DATABASE_URL"], f"postgres://agent_test:@agent-testdb:5432/{database}")
                self.assertEqual(step["env"]["PAPERCLIP_RUN_SCRATCH_DIR"], "${{ runner.temp }}/" + scratch)
        self.assertEqual(publication["env"]["TWO_BOT_TEST_BACKUP_BIN"], "${{ github.workspace }}/target/debug/two-bot")
        docs = self.step(DOC_STEP)
        self.assertEqual(docs["env"]["RUSTDOCFLAGS"], "-D warnings")
        self.assertEqual(shlex.split(docs["run"]), ["cargo", "doc", "--workspace", "--all-features", "--no-deps", "--locked"])

    def test_each_guarded_backup_and_rustdoc_command_propagates_a_subprocess_failure(self):
        with tempfile.TemporaryDirectory(prefix="nightly-step-failure-") as directory:
            temporary = Path(directory)
            cargo = temporary / "cargo"
            cargo.write_text("#!/bin/sh\nprintf 'fixture cargo failure\\n' >&2\nexit 47\n")
            cargo.chmod(0o755)
            env = {"PATH": str(temporary) + os.pathsep + os.environ["PATH"], "HOME": str(temporary)}
            for name in [row[0] for row in GUARDED_SUITES] + list(BACKUP_STEPS) + [DOC_STEP]:
                with self.subTest(step=name):
                    step = self.step(name)
                    scoped_env = env | {key: render(value, self.context()) for key, value in step.get("env", {}).items()}
                    result = subprocess.run(["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", render(step["run"], self.context())],
                                            cwd=temporary, env=scoped_env, text=True, capture_output=True, timeout=10)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn("fixture cargo failure", result.stderr)

    def test_missing_selector_output_selects_all_legs_and_only_explicit_false_deselects(self):
        for selected in (None, "", "true", "false", "False", "false ", "unknown"):
            with self.subTest(selected=selected):
                context = self.context()
                context["needs"]["changes"]["outputs"] = {} if selected is None else {"rust": selected}
                self.assertEqual(condition(self.sweep.get("if"), context), selected != "false")

    def test_aggregate_explicitly_needs_changes_and_the_complete_matrix(self):
        self.assertTrue("sweep-result" in self.workflow["jobs"], "nightly requires a final sweep-result aggregate")
        aggregate = self.workflow["jobs"]["sweep-result"]
        needs = aggregate.get("needs", [])
        self.assertTrue({"changes", "sweep"}.issubset(set(needs if isinstance(needs, list) else [needs])))
        self.assertEqual(aggregate.get("continue-on-error", "false"), "false")

    def aggregate_result(self, context):
        aggregate = self.workflow["jobs"]["sweep-result"]
        dependencies_failed = any(need["result"] != "success" for need in context["needs"].values())
        if not condition(aggregate.get("if"), context | {"_failed": dependencies_failed}):
            return None, "aggregate skipped"
        with tempfile.TemporaryDirectory(prefix="nightly-aggregate-") as directory:
            temporary = Path(directory)
            env = {"PATH": os.environ["PATH"], "HOME": str(temporary), "RUNNER_TEMP": str(temporary),
                   "GITHUB_OUTPUT": str(temporary / "output"), "GITHUB_STEP_SUMMARY": str(temporary / "summary")}
            env.update({key: render(value, context) for key, value in aggregate.get("env", {}).items()})
            ran, failed, output = 0, False, ""
            for step in aggregate.get("steps", []):
                scoped_context = context | {"_failed": failed, "env": env}
                if "run" not in step or not condition(step.get("if"), scoped_context):
                    continue
                scoped_env = env | {key: render(value, scoped_context) for key, value in step.get("env", {}).items()}
                result = subprocess.run(["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", render(step["run"], scoped_context)],
                                        cwd=temporary, env=scoped_env, text=True, capture_output=True, timeout=10)
                ran += 1
                failed |= result.returncode != 0 and step.get("continue-on-error", "false") != "true"
                output += result.stdout + result.stderr
            self.assertGreater(ran, 0, "aggregate must execute a verdict, not silently skip it")
            return not failed, output

    def test_aggregate_fails_closed_for_every_dependency_and_selection_result(self):
        self.assertTrue("sweep-result" in self.workflow["jobs"], "nightly requires a final sweep-result aggregate")
        for changes in ("success", "failure", "cancelled", "skipped"):
            for selected in ("", "true", "false", "unknown"):
                for sweep in ("success", "failure", "cancelled", "skipped"):
                    with self.subTest(changes=changes, selected=selected, sweep=sweep):
                        context = self.context()
                        context["needs"] = {"changes": {"result": changes, "outputs": {"rust": selected}}, "sweep": {"result": sweep}}
                        actual, output = self.aggregate_result(context)
                        deselected = changes == "success" and selected == "false" and sweep in ("success", "skipped")
                        expected = changes == "success" and (sweep == "success" or deselected)
                        # Explicit false may intentionally skip the verdict; failed
                        # dependencies and selected-but-skipped legs may never do so.
                        if actual is None:
                            self.assertTrue(deselected, output)
                        else:
                            self.assertEqual(actual, expected, output)

    def select_changes(self, paths, *, event="pull_request", base="a" * 40, diff_failure=False):
        matches = [step for step in self.workflow["jobs"]["changes"]["steps"] if step.get("id") == "select"]
        self.assertEqual(len(matches), 1)
        with tempfile.TemporaryDirectory(prefix="nightly-selector-") as directory:
            temporary = Path(directory)
            git = temporary / "git"
            git.write_text(f"#!{sys.executable}\nimport json, os, sys\n"
                           "if sys.argv[1:2] != ['diff']: sys.exit(97)\n"
                           "if os.environ['NIGHTLY_DIFF_FAILURE'] == '1': sys.exit(42)\n"
                           "sys.stdout.write(''.join(name + '\\0' for name in json.loads(os.environ['NIGHTLY_CHANGED_FILES'])))\n")
            git.chmod(0o755)
            output = temporary / "output"
            env = {"PATH": str(temporary) + os.pathsep + os.environ["PATH"], "HOME": str(temporary),
                   "EVENT_NAME": event, "BASE_SHA": base, "HEAD_SHA": "b" * 40, "GITHUB_OUTPUT": str(output),
                   "NIGHTLY_DIFF_FAILURE": "1" if diff_failure else "0", "NIGHTLY_CHANGED_FILES": json.dumps(paths)}
            result = subprocess.run(["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", matches[0]["run"]],
                                    cwd=temporary, env=env, text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertTrue(output.is_file(), "selector must publish an explicit decision")
            return dict(line.split("=", 1) for line in output.read_text().splitlines())

    def test_runner_and_acceptance_changes_trigger_nightly_without_rust_source_changes(self):
        for path in ("scripts/ci/run-nightly-partition.py", "scripts/ci/test_nightly_partition_acceptance.py",
                     "scripts/test_nightly_routing.py", "Cargo.lock"):
            with self.subTest(path=path):
                self.assertEqual(self.select_changes([path])["rust"], "true")
        self.assertEqual(self.select_changes(["docs/example.md"])["rust"], "false")

    def test_selector_defaults_to_full_sweep_for_schedule_dispatch_missing_base_and_diff_failure(self):
        for options in ({"event": "schedule"}, {"event": "workflow_dispatch"}, {"base": ""}, {"diff_failure": True}):
            with self.subTest(options=options):
                self.assertEqual(self.select_changes(["docs/example.md"], **options)["rust"], "true")

    def test_missing_acceptance_file_cannot_be_a_zero_test_green(self):
        steps = [step for step in self.sweep["steps"] if "test_nightly_partition_acceptance.py" in step.get("run", "")]
        self.assertTrue(steps, "nightly must execute its acceptance suite")
        with tempfile.TemporaryDirectory(prefix="nightly-missing-acceptance-") as directory:
            temporary = Path(directory)
            (temporary / "scripts/ci").mkdir(parents=True)
            python = temporary / "python3"
            python.write_text(f"#!{sys.executable}\nimport os, sys\n"
                              "if any('test_nightly_partition_acceptance.py' in arg for arg in sys.argv[1:]):\n"
                              f"    os.execv({sys.executable!r}, [{sys.executable!r}, *sys.argv[1:]])\n"
                              "sys.exit(0)\n")
            python.chmod(0o755)
            env = {"PATH": str(temporary) + os.pathsep + os.environ["PATH"], "HOME": str(temporary), "PYTHONDONTWRITEBYTECODE": "1"}
            failed, ran = False, False
            for step in steps:
                if condition(step.get("if"), self.context()):
                    result = subprocess.run(["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", render(step["run"], self.context())],
                                            cwd=temporary, env=env, text=True, capture_output=True, timeout=10)
                    failed |= result.returncode != 0
                    ran = True
            self.assertTrue(ran, "acceptance suite must be runnable on partition 0")
            self.assertTrue(failed, "missing acceptance file was silently accepted as zero tests")


class RealCargoHarnessAcceptance(unittest.TestCase):
    def exercise_real_suite_offline(self, *, refuse=False):
        # Replace only build admission and Cargo, not the harness or real runner.
        with tempfile.TemporaryDirectory(prefix="nightly-harness-acceptance-") as directory:
            temporary = Path(directory)
            root = temporary / "repo"
            (root / "scripts").mkdir(parents=True)
            (root / "rust-toolchain.toml").write_bytes((ROOT / "rust-toolchain.toml").read_bytes())
            real_bin = temporary / "original-path"
            real_bin.mkdir()
            cargo = real_bin / "cargo"
            fake = FAKE_CARGO.replace(
                'args = sys.argv[1:]',
                'if "fn main(_argument: i32)" in Path("ordinary.rs").read_text():\n'
                '    fixture["targets"][-1]["normal_compile_error"] = "E0580"\n'
                'args = sys.argv[1:]')
            fake = fake.replace(
                '            record(event)\n',
                '            record(event)\n'
                '            if test["name"] == "eligible_test":\n'
                '                print("ELIGIBLE_EXAMPLE_TEST_RAN")\n'
                '                if os.environ.get("NIGHTLY_EXAMPLE_MARKER"):\n'
                '                    with Path(os.environ["NIGHTLY_EXAMPLE_MARKER"]).open("a") as marker:\n'
                '                        marker.write("eligible\\n")\n')
            cargo.write_text(fake)
            cargo.chmod(0o755)
            wrapper = root / "scripts/cargo_cache.py"
            wrapper.write_text(
                f'#!{sys.executable}\nimport json, os, pathlib, subprocess, sys\n'
                'with pathlib.Path(os.environ["NIGHTLY_POOL_LEDGER"]).open("a") as log:\n'
                '    log.write(json.dumps({"args": sys.argv[1:], "path": os.environ["PATH"]}) + "\\n")\n'
                'if sys.argv[1:3] != ["run", "--"]: sys.exit(97)\n'
                'if os.environ["NIGHTLY_POOL_REFUSE"] == "1":\n'
                '    print("fixture bounded pool refused admission", file=sys.stderr)\n'
                '    sys.exit(75)\n'
                f'if os.environ["PATH"].split(os.pathsep)[0] != {str(real_bin)!r}: sys.exit(96)\n'
                'sys.exit(subprocess.run(["cargo", *sys.argv[3:]]).returncode)\n')
            inventory = temporary / "fixture.json"
            inventory.write_text(json.dumps({
                "targets": [
                    {"id": "fixture:lib", "kind": "lib", "tests": [{"name": "unit"}]},
                    {"id": "fixture:eligible", "kind": "example", "test": True,
                     "tests": [{"name": "eligible_test"}]},
                    {"id": "fixture:ordinary", "kind": "example", "tests": [{"name": "ordinary_test"}]},
                ], "doctests": [],
            }))
            ledger = temporary / "cargo.jsonl"
            pool_ledger = temporary / "pool.jsonl"
            environment = {
                "PATH": str(real_bin) + os.pathsep + os.environ["PATH"],
                "NIGHTLY_FIXTURE": str(inventory), "NIGHTLY_LEDGER": str(ledger),
                "NIGHTLY_POOL_LEDGER": str(pool_ledger), "NIGHTLY_POOL_REFUSE": "1" if refuse else "0",
            }
            output = io.StringIO()
            with patch.dict(globals(), {"ROOT": root}), patch.dict(os.environ, environment):
                with patch("sys.stdout", output):
                    result = unittest.TextTestRunner(stream=output, verbosity=2).run(real_cargo_suite())
            events = [json.loads(line) for line in ledger.read_text().splitlines()] if ledger.exists() else []
            admissions = [json.loads(line) for line in pool_ledger.read_text().splitlines()] if pool_ledger.exists() else []
            return result, output.getvalue(), events, admissions

    def test_real_cargo_harness_uses_the_supported_interface_without_compiling(self):
        result, output, events, admissions = self.exercise_real_suite_offline()
        self.assertTrue(result.wasSuccessful(), output)
        commands = [event for event in events if event["event"] == "command"]
        self.assertEqual(len(admissions), len(commands), "every Cargo call must pass through bounded admission")
        self.assertGreater(len(admissions), 4, "exercise baseline, partitioned execution and both invalid-example paths")
        executions = Counter(event["name"] for event in events if event["event"] == "execute")
        self.assertEqual(executions, Counter({"unit": 2, "eligible_test": 2}), output)
        self.assertIn("E0580", output)
        self.assertNotIn("expected the unchanged nightly cargo test command", output)

    def test_real_cargo_harness_stops_after_one_refused_admission(self):
        result, output, events, admissions = self.exercise_real_suite_offline(refuse=True)
        self.assertFalse(result.wasSuccessful(), "pool refusal must not become a green integration test")
        self.assertEqual(len(result.failures), 1, output)
        self.assertEqual(len(result.errors), 0, output)
        self.assertIn("bounded Cargo pool refused admission", output)
        self.assertEqual(len(admissions), 1, "refusal must not trigger retries or a fallback")
        self.assertFalse(events, "no compiler may run after refused admission")


def real_cargo_suite():
    """Explicit integration mode: every compilation uses the existing bounded pool."""
    class RealCargoExamples(unittest.TestCase):
        def test_original_default_example_selection_and_normal_build_errors(self):
            wrapper = ROOT / "scripts/cargo_cache.py"
            with tempfile.TemporaryDirectory(prefix="nightly-real-cargo-",
                                             dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as directory:
                temporary = Path(directory)
                (temporary / "Cargo.toml").write_text(
                    '[workspace]\n[package]\nname = "nightly-example-acceptance"\nversion = "0.0.0"\nedition = "2024"\n'
                    '[lib]\npath = "lib.rs"\n'
                    '[[example]]\nname = "ordinary"\npath = "ordinary.rs"\n'
                    '[[example]]\nname = "eligible"\npath = "eligible.rs"\ntest = true\n')
                (temporary / "Cargo.lock").write_text(
                    'version = 4\n[[package]]\nname = "nightly-example-acceptance"\nversion = "0.0.0"\n')
                (temporary / "rust-toolchain.toml").write_bytes((ROOT / "rust-toolchain.toml").read_bytes())
                (temporary / "lib.rs").write_text('#[test]\nfn unit() {}\n')
                ordinary = temporary / "ordinary.rs"
                ordinary.write_text(
                    'fn main() { panic!("ORDINARY_MAIN_MUST_NOT_RUN"); }\n'
                    '#[test]\nfn ordinary_test() { panic!("ORDINARY_TEST_MUST_NOT_RUN"); }\n')
                (temporary / "eligible.rs").write_text(
                    'fn main() { panic!("ELIGIBLE_MAIN_MUST_NOT_RUN"); }\n'
                    '#[test]\nfn eligible_test() {\n'
                    '    use std::io::Write;\n'
                    '    let path = std::env::var("NIGHTLY_EXAMPLE_MARKER").expect("fixture marker path");\n'
                    '    let mut marker = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();\n'
                    '    writeln!(marker, "eligible").unwrap();\n'
                    '}\n')
                refused = temporary / "pool-refused"
                marker = temporary / "example-executions"
                cargo = temporary / "bin/cargo"
                cargo.parent.mkdir()
                # Admission must resolve Cargo on the original PATH, not recurse into this shim.
                cargo.write_text(
                    f'#!{sys.executable}\nimport os, pathlib, subprocess, sys\n'
                    'environment = os.environ.copy()\n'
                    f'environment["PATH"] = {os.environ["PATH"]!r}\n'
                    f'result = subprocess.run([{sys.executable!r}, {str(wrapper)!r}, "run", "--", *sys.argv[1:]], env=environment)\n'
                    f'if result.returncode == 75: pathlib.Path({str(refused)!r}).touch()\n'
                    'sys.exit(result.returncode)\n')
                cargo.chmod(0o755)
                environment = os.environ.copy()
                environment["PATH"] = str(cargo.parent) + os.pathsep + environment["PATH"]
                environment["NIGHTLY_EXAMPLE_MARKER"] = str(marker)
                original = ["cargo", "test", *CARGO_FLAGS, "--", "--include-ignored", "--test-threads=1"]
                partitioned = [sys.executable, str(RUNNER), "--index", "0", "--count", "1", "--", *original]

                def run(command):
                    print("real-Cargo command:", shlex.join(command), flush=True)
                    result = subprocess.run(command, cwd=temporary, env=environment,
                                            text=True, capture_output=True, timeout=180)
                    output = result.stdout + result.stderr
                    print("real-Cargo exit:", result.returncode, flush=True)
                    print(output, flush=True)
                    # Refusal is an unverified real-Cargo result, never a skip,
                    # a direct-Cargo fallback, or another admission attempt.
                    if refused.exists():
                        self.fail("bounded Cargo pool refused admission; no compilation claim:\n" + output)
                    return result, output

                baseline, output = run(original)
                self.assertEqual(baseline.returncode, 0, output)
                self.assertTrue(marker.is_file(), "manifest test=true example did not execute")
                self.assertEqual(marker.read_text().splitlines(), ["eligible"], output)
                self.assertNotIn("ORDINARY_TEST_MUST_NOT_RUN", output)
                result, output = run(partitioned)
                with self.subTest(example="valid default selection"):
                    self.assertEqual(result.returncode, 0, output)
                    self.assertEqual(marker.read_text().splitlines(), ["eligible", "eligible"], output)
                    self.assertNotIn("ORDINARY_TEST_MUST_NOT_RUN", output)

                ordinary.write_text('fn main(_argument: i32) {}\n')
                baseline, output = run(original)
                self.assertNotEqual(baseline.returncode, 0, "fixture must fail ordinary example compilation")
                self.assertIn("E0580", output)
                result, output = run(partitioned)
                with self.subTest(example="invalid ordinary main signature"):
                    self.assertNotEqual(result.returncode, 0, "test harness hid an ordinary example compile error")
                    self.assertIn("E0580", output)

    return unittest.defaultTestLoader.loadTestsFromTestCase(RealCargoExamples)


if __name__ == "__main__":
    if sys.argv[1:] == ["--real-cargo"]:
        raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(real_cargo_suite()).wasSuccessful())
    unittest.main()
