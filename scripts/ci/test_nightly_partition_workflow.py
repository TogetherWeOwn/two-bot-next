#!/usr/bin/env python3
"""Offline acceptance tests for the partitioned nightly sweep workflow.

The single `sweep` job is split into `sweep-plan`, `sweep-partition` (six fixed
shards), `sweep-guarded`, `sweep-docs` and a fail-closed aggregate that keeps
the required check name `nightly ignored tests and docs`. The constants below
snapshot the original single-job workflow, so the split can neither narrow nor
broaden what the nightly sweep runs. Cargo flags, `--no-run` and target
selection live in `.github/scripts/nightly_partition.py`; these tests read that
helper's source and exercise its aggregate, instead of pinning invented cargo
invocations in the workflow.
"""

import ast
import fnmatch
import importlib.util
import json
import re
import tempfile
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/nightly.yml"
HELPER_PATH = ROOT / ".github/scripts/nightly_partition.py"
HELPER = "nightly_partition.py"

AGG_NAME = "nightly ignored tests and docs"
PARTITIONS = ["0", "1", "2", "3", "4", "5"]
COMPILING = ("sweep-plan", "sweep-partition", "sweep-guarded", "sweep-docs")
TESTING = ("sweep-partition", "sweep-guarded", "sweep-docs")
SWEEP_NEEDS = {"changes", "sweep-plan", "sweep-partition", "sweep-guarded", "sweep-docs"}
BROAD_STEP = "Full workspace sweep including ignored tests"
# All split legs use the same explicit standard public-hosted runner.
PUBLIC_RUNNER = "ubuntu-latest"

TIMEOUT_EXPR = "${{ (github.event_name == 'schedule' || github.event_name == 'workflow_dispatch') && 180 || 45 }}"
CONTAINER_IMAGE = 'rust:1.99-trixie@sha256:09027f873d45b8ecd1dc86f1fa13b7a62b875c961f017a0e331267b350430137'
POSTGRES_IMAGE = 'postgres:18.6@sha256:5a5a84b19854a9ffaa54082c166ff4ec27473a361e496e5ea167f298f2da9722'
REQUIRED_ENV = {'TWO_LFG_TESTDB_CI': '1',
 'TWO_GATEWAY_TEST_DATABASE_URL': 'postgresql://agent_test@localhost:5432/agent_test',
 'TWO_DATABASE_TLS': 'local-only',
 'CARGO_INCREMENTAL': '0'}
SELECTOR_RUNS_ON = PUBLIC_RUNNER
SELECTOR_OUTPUTS = {'rust': '${{ steps.select.outputs.rust }}', 'supply': '${{ steps.select.outputs.supply }}'}
SELECTOR_RUN = ('set -euo pipefail\n'
 'rust=true supply=true\n'
 'if [ "$EVENT_NAME" = "pull_request" ] \\\n'
 '  && [ -n "${BASE_SHA:-}" ] && [ -n "${HEAD_SHA:-}" ]; then\n'
 '  # The nightly suite runs on its schedule, not per PR: the required\n'
 '  # `check` matrix already runs the affected suites on every PR, and\n'
 '  # this 45-minute sweep on each push cost ~170 runs/day (most of them\n'
 '  # cancelled by the next push). A PR runs it only when it changes\n'
 '  # the nightly wiring or trips a CI-standard Rule 4 full-run trigger\n'
 '  # (.github/**, Cargo manifests/lock, toolchain, deny config); job-level\n'
 '  # gating keeps this workflow free of workflow-level paths filters.\n'
 '  # CI standard: --no-renames -z, and a diff error fails closed (full run).\n'
 '  if ! changed=$(git diff --name-only --no-renames -z "$BASE_SHA" "$HEAD_SHA" | tr \'\\0\' '
 "'\\n'); then\n"
 '    :\n'
 '  elif printf \'%s\\n\' "$changed" \\\n'
 '    | grep -qxE '
 "'\\.github/.*|(.*/)?Cargo\\.(toml|lock)|rust-toolchain(\\.toml)?|\\.cargo/.*|deny\\.toml|scripts/(job-inputs|test_nightly_routing)\\.py'; "
 'then\n'
 '    :\n'
 '  else\n'
 '    rust=false supply=false\n'
 '  fi\n'
 'fi\n'
 'printf \'rust=%s\\nsupply=%s\\n\' "$rust" "$supply" >>"$GITHUB_OUTPUT"\n')

# Guarded steps of the original sweep: (name, env, normalized command, if).
GUARDED = [('Channel moderation ignored tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-core --all-features --lib --locked channel_moderation_store:: -- '
  '--include-ignored --test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Channel moderation runtime acceptance with its guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test@agent-testdb:5432/agent_test'},
  'env -u PGPASSWORD -u PGPASSFILE -u PGOPTIONS cargo test -p two-bot-discord --all-features '
  '--test channel_moderation_runtime --locked -- --include-ignored --test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Discord channel-moderation executor ignored tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-discord --all-features --test internal_channel_moderation --locked -- '
  '--include-ignored --test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Mutation-429 channel acceptance ignored tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-discord --all-features --test mutation_429_proves_no_effect_channel '
  '--locked -- --include-ignored --test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Feed store ignored tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-core --all-features --test feeds_store --locked -- --include-ignored '
  '--test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Legacy verification ignored tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-cutover --all-features --test legacy_verify_db --locked -- '
  '--include-ignored --test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Legacy TLS fence ignored tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-cutover --all-features --test legacy_tls_fence --locked -- '
  '--include-ignored --test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Reengagement list ignored tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test:@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-cutover --all-features --test reengagement_list_db --locked -- '
  '--include-ignored --test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Game-activity report tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test:@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-cutover --all-features --test game_activity_report_db --locked -- '
  '--test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Staging migration runner ignored tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-cutover --all-features --test staging_migrate_db --locked -- '
  '--include-ignored --test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Internal events ignored tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test:@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-discord --all-features --test internal_events --locked -- '
  '--include-ignored --test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Raid list ignored tests with their guarded URL',
  {'TWO_TEST_DATABASE_URL': 'postgres://agent_test:@agent-testdb:5432/agent_test'},
  'cargo test -p two-bot-cutover --all-features --test raid_list_db --locked -- --include-ignored '
  '--test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Build backup CLI',
  {},
  'cargo build -p two-bot --bin two-bot --all-features --locked',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Backup roundtrip on a dedicated disposable database',
  {'TWO_BOT_TEST_DATABASE_URL': 'postgres://agent_test:@agent-testdb:5432/two_next_backup_nightly',
   'PAPERCLIP_RUN_SCRATCH_DIR': '${{ runner.temp }}/backup-roundtrip'},
  'cargo test -p two-bot-core --all-features --test backup_roundtrip --locked -- --include-ignored '
  '--test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' }}"),
 ('Backup publication on a dedicated disposable database',
  {'TWO_BOT_TEST_DATABASE_URL': 'postgres://agent_test:@agent-testdb:5432/two_next_backup_publication_nightly',
   'TWO_BOT_TEST_BACKUP_BIN': '${{ github.workspace }}/target/debug/two-bot',
   'PAPERCLIP_RUN_SCRATCH_DIR': '${{ runner.temp }}/backup-publication'},
  'cargo test -p two-bot-core --all-features --test backup_dump_publication --locked -- '
  '--include-ignored --test-threads=1',
  "${{ !cancelled() && steps.databases.outcome == 'success' && steps.backup-cli.outcome == "
  "'success' }}")]

# Verbatim, in order (including the one duplicate), from the original broad step.
ORIGINAL_SKIPS = ['regenerate_reference_docs',
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
 'event_store_s7_privacy_tripwire_rejects_descriptions_and_locations']


class Loader(yaml.BaseLoader):
    def construct_mapping(self, node, deep=False):
        mapping = {}
        for key_node, value_node in node.value:
            key = self.construct_object(key_node, deep=deep)
            if key in mapping:
                raise ValueError(f"duplicate workflow key: {key}")
            mapping[key] = self.construct_object(value_node, deep=deep)
        return mapping


def norm(text):
    return " ".join(text.split())


def runs(job):
    return [norm(s["run"]) for s in job.get("steps", []) if "run" in s]


def uses(job, action):
    return [s for s in job.get("steps", []) if action in s.get("uses", "")]


def load_helper():
    spec = importlib.util.spec_from_file_location("nightly_partition_under_test", HELPER_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def helper_steps(job):
    return [s for s in job.get("steps", []) if HELPER in s.get("run", "")]


class NightlyPartitionWorkflow(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()
        cls.wf = yaml.load(cls.text, Loader=Loader)
        cls.jobs = cls.wf["jobs"]

    def job(self, name):
        if name not in self.jobs:
            self.fail(f"missing job {name}")
        return self.jobs[name]

    def all_runs(self, name):
        return " \n".join(runs(self.job(name)))

    def needs(self, name):
        need = self.job(name)["needs"]
        return {need} if isinstance(need, str) else set(need)

    # --- selector -----------------------------------------------------
    def test_changes_selector_is_unchanged(self):
        job = self.job("changes")
        self.assertEqual(job["runs-on"], SELECTOR_RUNS_ON)
        self.assertEqual(job["outputs"], SELECTOR_OUTPUTS)
        self.assertEqual(job["steps"][1]["run"], SELECTOR_RUN)

    # --- topology -----------------------------------------------------
    def test_job_needs(self):
        self.assertEqual(self.needs("sweep-plan"), {"changes"})
        self.assertEqual(self.needs("sweep-partition"), {"changes", "sweep-plan"})
        for name in ("sweep-guarded", "sweep-docs"):
            self.assertIn("changes", self.needs(name), name)
        self.assertEqual(self.needs("sweep"), SWEEP_NEEDS)

    def test_compiling_jobs_gate_on_rust_selector(self):
        for name in COMPILING:
            cond = self.job(name)["if"]
            self.assertIn("!cancelled()", cond, name)
            self.assertIn("needs.changes.outputs.rust != 'false'", cond, name)

    def test_partition_matrix_is_fixed_and_bounded(self):
        strat = self.job("sweep-partition")["strategy"]
        self.assertEqual(strat["matrix"]["partition"], PARTITIONS)
        self.assertEqual(strat["fail-fast"], "false")
        self.assertEqual(strat["max-parallel"], "2")
        self.assertEqual(set(strat["matrix"]), {"partition"}, "no extra matrix axes")

    # --- shared compiling-job contract ----------------------------------
    def test_compiling_jobs_keep_timeout_container_env_and_routing(self):
        self.assertIn("180", TIMEOUT_EXPR)
        self.assertIn("45", TIMEOUT_EXPR)
        for name in COMPILING:
            job = self.job(name)
            self.assertEqual(job["timeout-minutes"], TIMEOUT_EXPR, name)
            self.assertEqual(job["container"]["image"], CONTAINER_IMAGE, name)
            self.assertEqual(job["runs-on"], PUBLIC_RUNNER, name)
            self.assertNotIn("CI_OVERFLOW_", job["runs-on"], name)
            self.assertNotIn("self-hosted", job["runs-on"], name)
            self.assertEqual(job["permissions"], {"contents": "read"}, name)
            for key, value in REQUIRED_ENV.items():
                self.assertEqual(job["env"].get(key), value, f"{name} {key}")

    def test_testing_jobs_keep_disposable_database_service(self):
        for name in TESTING:
            svc = self.job(name)["services"]["agent-testdb"]
            self.assertEqual(svc["image"], POSTGRES_IMAGE, name)
            self.assertEqual(svc["env"]["POSTGRES_USER"], "agent_test", name)
            self.assertEqual(svc["env"]["POSTGRES_DB"], "agent_test", name)
            self.assertEqual(svc["env"]["POSTGRES_HOST_AUTH_METHOD"], "trust", name)
            self.assertNotIn("ports", svc, name)
            self.assertNotIn("--test-threads=2", self.all_runs(name))

    def test_compiling_jobs_install_pinned_toolchain_and_commandref(self):
        for name in COMPILING:
            job = self.job(name)
            tc = uses(job, "dtolnay/rust-toolchain")
            self.assertTrue(tc, name)
            self.assertEqual(tc[0]["with"]["toolchain"], "1.99.1", name)
            self.assertIn("generate_command_reference.py", self.all_runs(name), name)
            # Generation must precede the first step that compiles or runs tests.
            names = [norm(s.get("run", "")) for s in job["steps"]]
            gen = next(i for i, r in enumerate(names) if "generate_command_reference.py" in r)
            first = next(i for i, r in enumerate(names)
                         if "cargo " in r or HELPER in r)
            self.assertLess(gen, first, name)

    # --- plan: the only place with the verbatim skip flags -------------------
    def test_plan_step_carries_original_skip_flags_verbatim(self):
        job = self.job("sweep-plan")
        steps = [s for s in job["steps"] if s.get("name") == BROAD_STEP]
        self.assertEqual(len(steps), 1)
        run = norm(steps[0]["run"])
        self.assertIn(f"python3 .github/scripts/{HELPER} plan -- --include-ignored --test-threads=1 --skip", run)
        self.assertNotIn("cargo", run, "cargo flags and --no-run live in the helper")
        self.assertEqual(re.findall(r"--skip (\S+)", run), ORIGINAL_SKIPS)
        self.assertEqual(len(re.findall(r"--skip ", run)), len(ORIGINAL_SKIPS))
        self.assertTrue(uses(job, "actions/upload-artifact"), "plan must be uploaded")

    def test_only_plan_has_skip_flags(self):
        for name in self.jobs:
            if name in ("sweep-plan", "sweep-guarded"):
                continue
            for run in runs(self.job(name)):
                self.assertNotIn("--skip", run, f"{name}: skip lists must not be duplicated")
        for run in runs(self.job("sweep-guarded")):
            self.assertNotIn("--skip", run, "guarded steps are un-skipped dedicated routes")
        plan_skips = [r for r in runs(self.job("sweep-plan")) if "--skip" in r]
        self.assertEqual(len(plan_skips), 1)

    def test_partition_and_docs_use_plan_test_args_through_the_helper(self):
        part = helper_steps(self.job("sweep-partition"))
        self.assertEqual(len(part), 1)
        run = norm(part[0]["run"])
        self.assertIn(f"{HELPER} run", run)
        # The matrix value reaches the shell through env, never interpolated into the script.
        self.assertEqual(part[0]["env"]["NIGHTLY_PARTITION"], "${{ matrix.partition }}")
        self.assertNotIn("${{", part[0]["run"])
        self.assertIn('--partition "$NIGHTLY_PARTITION"', run)
        self.assertIn("--receipt", run)
        self.assertNotIn("cargo", run)
        self.assertNotIn("--include-ignored", run)
        self.assertNotIn("--no-run", run)
        docs = helper_steps(self.job("sweep-docs"))
        self.assertEqual(len(docs), 1)
        drun = norm(docs[0]["run"])
        self.assertIn(f"{HELPER} docs", drun)
        self.assertNotIn("cargo", drun)
        self.assertNotIn("--include-ignored", drun)
        self.assertNotIn("--no-run", drun)
        # Doctests previously ran inside the broad sweep's step-scoped environment.
        original = next(s for s in self.job("sweep-plan")["steps"]
                        if s.get("name") == BROAD_STEP)["env"]
        self.assertEqual(docs[0]["env"], original)
        self.assertEqual({k: v for k, v in part[0]["env"].items() if k != "NIGHTLY_PARTITION"}, original)

    def test_docs_job_builds_docs_strictly(self):
        job = self.job("sweep-docs")
        step = [s for s in job["steps"] if "cargo doc" in s.get("run", "")]
        self.assertEqual(len(step), 1)
        run = norm(step[0]["run"])
        for flag in ("--workspace", "--all-features", "--no-deps", "--locked"):
            self.assertIn(flag, run)
        self.assertIn("-D warnings", step[0]["env"]["RUSTDOCFLAGS"])
        self.assertIn("!cancelled()", step[0].get("if", ""))

    def test_plan_download_precedes_all_expensive_setup(self):
        for name in ("sweep-partition", "sweep-docs"):
            with self.subTest(job=name):
                steps = self.job(name)["steps"]
                self.assertIn("actions/checkout@", steps[0]["uses"])
                download = steps[1]
                self.assertEqual(download.get("name"), "Download exact-run inventory plan")
                self.assertEqual(download.get("id"), "plan")
                self.assertEqual(download.get("uses"),
                                 "actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333")
                self.assertEqual(download.get("with"), {"name": "nightly-plan"})
                self.assertNotIn("if", download)
                self.assertNotIn("continue-on-error", download)

    def test_missing_plan_skips_setup_and_every_compiling_step(self):
        for name in ("sweep-partition", "sweep-docs"):
            for step in self.job(name)["steps"]:
                run = step.get("run", "")
                expensive = ("apt-get" in run or "createdb" in run
                             or "generate_command_reference.py" in run
                             or HELPER in run or "cargo " in run
                             or "rust-toolchain@" in step.get("uses", "")
                             or "actions/cache@" in step.get("uses", ""))
                if not expensive:
                    continue
                with self.subTest(job=name, step=step.get("name")):
                    if "cargo doc" in run:
                        # A status function avoids implicit success(): docs still
                        # run after doctest failure, but not a missing plan or cancellation.
                        self.assertEqual(step.get("if"),
                                         "${{ !cancelled() && steps.plan.outcome == 'success' }}")
                    else:
                        # The runner's implicit success() skips all setup/test work
                        # after the early download fails; no failure-path override.
                        self.assertNotIn("if", step)
                    self.assertNotIn("continue-on-error", step)

    # --- helper source: the executable chain ------------------------------
    def helper_tree(self):
        return ast.parse(HELPER_PATH.read_text())

    def test_helper_cargo_chain_is_workspace_all_features_locked_default_targets(self):
        tree = self.helper_tree()
        cargo = None
        for node in ast.walk(tree):
            if isinstance(node, ast.Assign) and any(
                    isinstance(t, ast.Name) and t.id == "CARGO" for t in node.targets):
                cargo = ast.literal_eval(node.value)
        self.assertEqual(cargo, ["cargo", "test", "--workspace", "--all-features", "--locked"])
        constants = {n.value for n in ast.walk(tree)
                     if isinstance(n, ast.Constant) and isinstance(n.value, str)}
        self.assertIn("--no-run", constants)
        self.assertIn("--message-format=json", constants)
        self.assertIn("--doc", constants)
        # Default cargo test also compiles ordinary examples; never narrow it.
        for narrowing in ("--lib", "--bins", "--tests", "--all-targets", "--examples",
                          "--bin", "--test", "--package", "-p", "--exclude"):
            self.assertNotIn(narrowing, constants)
        # Every cargo spawn goes through CARGO or cargo metadata.
        calls = [n for n in ast.walk(tree) if isinstance(n, ast.Constant) and n.value == "cargo"]
        self.assertLessEqual(len(calls), 2)

    def test_helper_validates_sha_and_local_inventory_before_running(self):
        source = HELPER_PATH.read_text()
        self.assertIn("GITHUB_SHA", source)
        h = load_helper()
        plan = self.make_plan(h, "a" * 40)
        h.validate_plan(plan, "a" * 40)
        with self.assertRaises(ValueError):
            h.validate_plan(plan, "b" * 40)
        # Tampered partition union is rejected, as is a wrong version.
        bad = json.loads(json.dumps(plan))
        bad["partitions"][0] = bad["partitions"][0][:-1]
        with self.assertRaises(ValueError):
            h.validate_plan(bad, "a" * 40)
        bad = dict(plan, version=2)
        with self.assertRaises(ValueError):
            h.validate_plan(bad, "a" * 40)
        # The run path compares fresh discovery with the plan, test args included.
        run_src = ast.get_source_segment(source, next(
            n for n in ast.walk(self.helper_tree())
            if isinstance(n, ast.FunctionDef) and n.name == "run_partition"))
        for needle in ("validate_plan", "discover", 'plan["inventory"]', 'plan["test_args"]'):
            self.assertIn(needle, run_src)
        self.assertLess(run_src.index("validate_plan"), run_src.index("discover"))
        # Docs mode reads test_args from the validated plan, never from the workflow.
        start = source.index('elif args.mode == "docs"')
        docs_src = source[start:source.index("\n    else:\n", start)]
        self.assertLess(docs_src.index("validate_plan"), docs_src.index('plan["test_args"]'))

    def test_helper_rejects_non_original_test_args(self):
        h = load_helper()
        base = ["--include-ignored", "--test-threads=1"]
        self.assertEqual(h.validate_args(base + ["--skip", "x"]), base + ["--skip", "x"])
        for bad in ([], ["--test-threads=1", "--include-ignored"], base + ["--nocapture"],
                    base + ["--skip"], base + ["--test-threads=4"], ["--include-ignored"]):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                h.validate_args(bad)

    # --- guarded --------------------------------------------------------
    def test_guarded_job_keeps_every_original_route(self):
        job = self.job("sweep-guarded")
        steps = {s.get("name"): s for s in job["steps"]}
        for name, env, cmd, cond in GUARDED:
            self.assertIn(name, steps, f"guarded step lost: {name}")
            step = steps[name]
            self.assertEqual(norm(step["run"]), cmd, name)
            self.assertEqual(step.get("env", {}), env, name)
            if cond is not None:
                self.assertIn("steps.databases.outcome == 'success'", step.get("if", ""), name)
        for _, _, cmd, _ in GUARDED:
            if "--include-ignored" in cmd or "--test-threads=1" in cmd:
                self.assertIn("--test-threads=1", cmd)

    def test_guarded_urls_are_disposable_agent_test_hosts(self):
        for step in self.job("sweep-guarded")["steps"]:
            for key, value in step.get("env", {}).items():
                if key.endswith("DATABASE_URL") and "@" in value:
                    self.assertIn("@agent-testdb:5432/", value)
                    self.assertTrue(value.startswith("postgres"), value)

    def test_guarded_backup_prerequisites(self):
        job = self.job("sweep-guarded")
        text = self.all_runs("sweep-guarded")
        for db in ("two_bot_test_tog10090_nightly", "two_next_backup_nightly",
                   "two_next_backup_publication_nightly"):
            self.assertIn(f"createdb -h agent-testdb -U agent_test {db}", text)
        self.assertIn("backup-roundtrip", text)
        self.assertIn("backup-publication", text)
        self.assertIn("mkdir -p", text)
        self.assertIn("socat", text)
        self.assertIn("test_nightly_routing.py", " ".join(
            runs(job) + runs(self.job("sweep-plan"))))
        names = [s.get("name") for s in job["steps"]]
        build = names.index("Build backup CLI")
        pub = names.index("Backup publication on a dedicated disposable database")
        self.assertLess(build, pub)
        cli = job["steps"][build]
        self.assertEqual(cli.get("id"), "backup-cli")
        self.assertIn("cargo build -p two-bot --bin two-bot --all-features --locked", norm(cli["run"]))
        pub_step = job["steps"][pub]
        self.assertIn("steps.backup-cli.outcome == 'success'", pub_step["if"])
        self.assertTrue(pub_step["env"]["TWO_BOT_TEST_BACKUP_BIN"].endswith("/target/debug/two-bot"))
        self.assertTrue(pub_step["env"]["PAPERCLIP_RUN_SCRATCH_DIR"].endswith("/backup-publication"))
        self.assertIn("databases", [s.get("id") for s in job["steps"]])

    # --- no failure tolerance ---------------------------------------------
    def test_no_failure_tolerance_anywhere(self):
        self.assertNotIn("continue-on-error", self.text)
        for name in COMPILING + ("sweep",):
            text = self.all_runs(name)
            self.assertNotRegex(text, r"\|\|\s*(true|:|exit 0)\b", name)
            self.assertNotIn("set +e", text, name)
            self.assertNotIn("--keep-going", text, name)
        source = HELPER_PATH.read_text()
        self.assertNotIn("check=False", source)

    # --- aggregate --------------------------------------------------------
    def test_aggregate_keeps_required_name_and_always_runs(self):
        job = self.job("sweep")
        self.assertEqual(job["name"], AGG_NAME)
        self.assertIn("always()", job["if"])
        self.assertNotIn("!cancelled()", job["if"])
        self.assertNotIn("continue-on-error", job)
        self.assertEqual(job["permissions"], {"contents": "read"})
        self.assertNotIn("container", job)
        self.assertNotIn("services", job)
        self.assertNotIn("cargo ", self.all_runs("sweep"))

    def test_aggregate_hands_needs_json_to_the_helper(self):
        job = self.job("sweep")
        carriers = [s for s in job["steps"] if s.get("env", {}).get("NIGHTLY_NEEDS") is not None]
        carriers += [None] if job.get("env", {}).get("NIGHTLY_NEEDS") is not None else []
        self.assertTrue(carriers, "NIGHTLY_NEEDS must be set from toJSON(needs)")
        for carrier in carriers:
            env = job["env"] if carrier is None else carrier["env"]
            self.assertEqual(env["NIGHTLY_NEEDS"], "${{ toJSON(needs) }}")
        step = [s for s in helper_steps(job) if "aggregate" in s["run"]]
        self.assertEqual(len(step), 1)
        run = step[0]["run"]
        self.assertIn("NIGHTLY_NEEDS", run)
        self.assertRegex(run, r"NIGHTLY_NEEDS\"?\s*>\s*(\S+\.json)")
        written = re.search(r"NIGHTLY_NEEDS\"?\s*>\s*(\S+\.json)", run).group(1)
        self.assertRegex(norm(run), rf"{HELPER} aggregate .*--needs {re.escape(written)}")
        # Statuses are judged by the helper, not by per-job workflow expressions.
        self.assertNotRegex(yaml.dump(job), r"needs\.[\w-]+\.result")
        self.assertEqual(step[0].get("if"), "always()")
        self.assertNotIn("result", " ".join(runs(job)).replace("nightly-receipts", ""))
        self.assertTrue(uses(job, "actions/checkout"), "helper lives in the repository")
        self.assertNotIn("|| true", run)

    def make_plan(self, h, sha):
        inventory = {
            "a|lib|a": [f"t{i}" for i in range(7)],
            "b|test|b": [f"u{i}" for i in range(9)],
        }
        return {"version": 1, "sha": sha, "inventory": inventory, "unfiltered": inventory,
                "test_args": ["--include-ignored", "--test-threads=1"],
                "partitions": h.partition(inventory)}

    def write_receipts(self, h, plan, directory, sha, skip=None):
        for i in range(6):
            if i == skip:
                continue
            (directory / f"partition-{i}.json").write_text(json.dumps({
                "version": 1, "sha": sha, "partition": i, "plan_digest": h.digest(plan),
                "executed": plan["partitions"][i]}))

    def needs_all(self, result="success", **override):
        needs = {name: {"result": result, "outputs": {}} for name in SWEEP_NEEDS}
        needs["changes"] = {"result": "success", "outputs": {"rust": "true"}}
        for name, value in override.items():
            needs[name.replace("_", "-")] = value
        return needs

    def test_helper_aggregate_is_fail_closed_with_six_receipts(self):
        h = load_helper()
        self.assertEqual(set(h.JOBS), SWEEP_NEEDS)
        self.assertEqual(h.PARTITIONS, 6)
        sha = "c" * 40
        plan = self.make_plan(h, sha)
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write_receipts(h, plan, d, sha)
            h.aggregate(self.needs_all(), plan, d, sha)  # all green passes
            for job in SWEEP_NEEDS - {"changes"}:
                for result in ("failure", "cancelled", "skipped"):
                    needs = self.needs_all()
                    needs[job]["result"] = result
                    with self.subTest(job=job, result=result), self.assertRaises(ValueError):
                        h.aggregate(needs, plan, d, sha)
            for result in ("failure", "cancelled", "skipped"):
                needs = self.needs_all()
                needs["changes"]["result"] = result
                with self.subTest(changes=result), self.assertRaises(ValueError):
                    h.aggregate(needs, plan, d, sha)
            extra = self.needs_all()
            extra["sweep-surprise"] = {"result": "success", "outputs": {}}
            with self.assertRaises(ValueError):
                h.aggregate(extra, plan, d, sha)
            missing = self.needs_all()
            del missing["sweep-docs"]
            with self.assertRaises(ValueError):
                h.aggregate(missing, plan, d, sha)
            with self.assertRaises(ValueError):
                h.aggregate(self.needs_all(), plan, d, "d" * 40)

    def test_helper_aggregate_requires_each_exact_receipt(self):
        h = load_helper()
        sha = "c" * 40
        plan = self.make_plan(h, sha)
        for lost in range(6):
            with tempfile.TemporaryDirectory() as tmp:
                d = Path(tmp)
                self.write_receipts(h, plan, d, sha, skip=lost)
                with self.subTest(missing=lost), self.assertRaises(ValueError):
                    h.aggregate(self.needs_all(), plan, d, sha)
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write_receipts(h, plan, d, sha)
            (d / "partition-6.json").write_text("{}")
            with self.assertRaises(ValueError):
                h.aggregate(self.needs_all(), plan, d, sha)
        for field, value in (("executed", []), ("sha", "e" * 40), ("plan_digest", "0" * 64),
                             ("partition", 1), ("version", 2)):
            with tempfile.TemporaryDirectory() as tmp:
                d = Path(tmp)
                self.write_receipts(h, plan, d, sha)
                path = d / "partition-0.json"
                receipt = json.loads(path.read_text())
                receipt[field] = value
                path.write_text(json.dumps(receipt))
                with self.subTest(field=field), self.assertRaises(ValueError):
                    h.aggregate(self.needs_all(), plan, d, sha)

    def test_helper_aggregate_deselection_requires_clean_skips(self):
        h = load_helper()
        sha = "c" * 40
        skipped = {n: {"result": "skipped", "outputs": {}} for n in SWEEP_NEEDS}
        skipped["changes"] = {"result": "success", "outputs": {"rust": "false"}}
        with tempfile.TemporaryDirectory() as tmp:
            h.aggregate(skipped, None, Path(tmp), sha)  # explicit deselection is fine
            for job in SWEEP_NEEDS - {"changes"}:
                for result in ("success", "failure", "cancelled"):
                    needs = json.loads(json.dumps(skipped))
                    needs[job]["result"] = result
                    with self.subTest(job=job, result=result), self.assertRaises(ValueError):
                        h.aggregate(needs, None, Path(tmp), sha)
            # A failed or output-less selector is never read as deselection.
            for outputs in ({}, {"rust": "true"}):
                needs = json.loads(json.dumps(skipped))
                needs["changes"]["outputs"] = outputs
                with self.subTest(outputs=outputs), self.assertRaises((ValueError, TypeError)):
                    h.aggregate(needs, None, Path(tmp), sha)

    # --- receipt / inventory wiring ---------------------------------------
    def effective_arg(self, run, flag, default):
        match = re.search(rf"{flag} (\S+)", run)
        return match.group(1).strip('"') if match else default

    def test_inventory_receipt_wiring(self):
        plan = self.job("sweep-plan")
        part = self.job("sweep-partition")
        agg = self.job("sweep")
        plan_run = " ".join(runs(plan))
        plan_file = self.effective_arg(plan_run, "--plan", "nightly-plan.json")
        up = uses(plan, "actions/upload-artifact")
        self.assertEqual(len(up), 1)
        self.assertEqual(up[0]["with"]["path"], plan_file)
        plan_artifact = up[0]["with"]["name"]
        self.assertEqual(plan_artifact, "nightly-plan", "stable name: same-run reruns reuse it")
        for name in ("sweep-partition", "sweep-docs", "sweep"):
            down = [s for s in uses(self.job(name), "actions/download-artifact")
                    if s["with"].get("name") == plan_artifact]
            self.assertEqual(len(down), 1, f"{name} must download this attempt's plan")
            run = " ".join(runs(self.job(name)))
            self.assertEqual(self.effective_arg(run, "--plan", "nightly-plan.json"), plan_file, name)
        # Each shard uploads exactly the receipt file the aggregate globs for.
        pup = uses(part, "actions/upload-artifact")
        self.assertEqual(len(pup), 1)
        self.assertEqual(pup[0].get("if", "").count("cancelled()"), 0)
        self.assertIn("always()", pup[0].get("if", "") or "always()")
        self.assertEqual(pup[0]["with"]["name"], "nightly-partition-${{ matrix.partition }}")
        self.assertTrue(pup[0]["with"]["path"].endswith("partition-${{ matrix.partition }}.json"))
        self.assertEqual(pup[0]["with"].get("if-no-files-found"), "error")
        prun = " ".join(runs(part))
        self.assertEqual(
            self.effective_arg(prun, "--receipt", "").replace("$NIGHTLY_PARTITION", "${{ matrix.partition }}"),
            pup[0]["with"]["path"])
        receipts_dir = str(Path(pup[0]["with"]["path"]).parent)
        # The aggregate gathers all six into one flat directory the helper reads.
        arun = " ".join(runs(agg))
        wanted = self.effective_arg(arun, "--receipts", "nightly-receipts")
        gather = [s for s in uses(agg, "actions/download-artifact")
                  if s["with"].get("name") != plan_artifact]
        self.assertEqual(len(gather), 1)
        self.assertEqual(gather[0]["with"]["path"], wanted)
        self.assertEqual(gather[0]["with"].get("merge-multiple"), "true")
        self.assertEqual(gather[0]["with"]["pattern"], "nightly-partition-*")
        self.assertEqual(wanted, receipts_dir)

    def artifact_steps(self):
        return [(name, s) for name in ("sweep-plan", "sweep-partition", "sweep-docs", "sweep")
                for s in self.job(name)["steps"]
                if "actions/upload-artifact" in s.get("uses", "")
                or "actions/download-artifact" in s.get("uses", "")]

    def test_artifact_identity_is_stable_across_same_run_reruns(self):
        # Failed-job reruns keep the run id, bump run_attempt, and re-execute
        # only some matrix legs. Names must therefore not embed the attempt (or
        # anything else that varies per attempt), or a rerun leg would upload
        # beside, not over, its predecessor and downloads would miss survivors.
        volatile = ("run_attempt", "github.sha", "github.run_id", "runner.", "github.run_number")
        steps = self.artifact_steps()
        self.assertTrue(steps)
        for job, step in steps:
            with_ = step["with"]
            for key in ("name", "pattern", "path"):
                for token in volatile:
                    self.assertNotIn(token, with_.get(key, ""), f"{job} {key}")
            # Default downloads are scoped to the current run: no cross-run lookup.
            for key in ("run-id", "github-token", "repository", "artifact-ids"):
                self.assertNotIn(key, with_, f"{job}: downloads must stay in this run")
        # Rendering the names for attempt 1 and attempt 2 yields identical sets.
        part_up = uses(self.job("sweep-partition"), "actions/upload-artifact")[0]["with"]["name"]
        rendered = lambda: [part_up.replace("${{ matrix.partition }}", n) for n in PARTITIONS]
        self.assertEqual(rendered(), rendered())
        self.assertEqual(len(set(rendered())), 6)
        pattern = [s for s in uses(self.job("sweep"), "actions/download-artifact")
                   if "pattern" in s["with"]][0]["with"]["pattern"]
        plan_name = uses(self.job("sweep-plan"), "actions/upload-artifact")[0]["with"]["name"]
        self.assertTrue(all(fnmatch.fnmatchcase(n, pattern) for n in rendered()))
        self.assertFalse(fnmatch.fnmatchcase(plan_name, pattern), "plan must not match receipts")

    def test_uploads_overwrite_so_failed_job_reruns_replace_stale_artifacts(self):
        uploads = [(job, s) for job, s in self.artifact_steps() if "upload-artifact" in s["uses"]]
        self.assertEqual({job for job, _ in uploads}, {"sweep-plan", "sweep-partition"})
        for job, step in uploads:
            self.assertEqual(step["with"].get("overwrite"), "true", job)
            self.assertEqual(step["with"].get("if-no-files-found"), "error", job)
        # Replacement is safe only because the helper judges contents: a stale
        # or foreign receipt is rejected by SHA and plan digest, never trusted.
        h = load_helper()
        sha = "c" * 40
        plan, other = self.make_plan(h, sha), self.make_plan(h, "d" * 40)
        with tempfile.TemporaryDirectory() as tmp:
            d = Path(tmp)
            self.write_receipts(h, other, d, "d" * 40)  # leftovers of a different plan
            with self.assertRaises(ValueError):
                h.aggregate(self.needs_all(), plan, d, sha)
            self.write_receipts(h, plan, d, sha)  # rerun overwrote every leg
            h.aggregate(self.needs_all(), plan, d, sha)
            stale = d / "partition-3.json"
            receipt = json.loads(stale.read_text())
            receipt["plan_digest"] = h.digest(other)
            stale.write_text(json.dumps(receipt))
            with self.assertRaises(ValueError):
                h.aggregate(self.needs_all(), plan, d, sha)

    def test_actions_are_sha_pinned(self):
        for name in COMPILING + ("sweep",):
            for step in self.job(name)["steps"]:
                ref = step.get("uses")
                if ref:
                    self.assertRegex(ref, r"@[0-9a-f]{40}\b", f"{name}: {ref}")


if __name__ == "__main__":
    unittest.main()
