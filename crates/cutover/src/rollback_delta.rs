//! Read-only Next-window delta report for rollback decisions (TOG-12022).
//!
//! `docs/cutover.md` notes that the forward upsert is not a rollback delta
//! exporter: rollback needs the full captured Next delta measured from the
//! `T_f` baseline. This module supplies the read-only measurement. The reverse
//! import lives in [`crate::rollback_import`] (`rollback-delta import`).
//!
//! For every bot-owned table the report counts rows inserted or updated after
//! `T_f`, using that table's timestamp column(s). Timestamp storage is mixed
//! across the migration chain (`timestamptz` in the cutover chain, ISO-8601
//! UTC text in the store chain), so every recency predicate casts to
//! `timestamptz`: the cast is a no-op on real timestamps and parses the
//! canonical text format. A table with no usable timestamp column is reported
//! as `unmeasurable` with a reason and a rollback disposition
//! (`accept-with-reason` or `manual-reconcile-required`, see
//! `docs/rollback-delta.md`), never silently skipped; a table absent
//! from the database is reported as `missing`.
//!
//! All reads run inside one `REPEATABLE READ, READ ONLY` transaction (see
//! [`crate::legacy_verify::read_only_transaction`]), so the per-table counts
//! share a single snapshot. Nothing here writes: migrations are never applied
//! by this path (the binary opens the database with `migrations_off`).

use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;

/// Report format version, bumped only for breaking JSON changes.
pub const REPORT_VERSION: u32 = 1;

/// Maximum rows exported per table by [`export_delta`]. A rollback window is
/// small by construction; an unbounded export would let a mis-set `--since`
/// spill the whole database into a file.
pub const MAX_EXPORT_ROWS_PER_TABLE: u64 = 50_000;

/// Rollback disposition for an `unmeasurable` table, recorded in the delta
/// report so a rollback decision never meets an unmeasurable table with no
/// named handling. See `docs/rollback-delta.md` for the semantics.
pub const DISPOSITION_ACCEPT_WITH_REASON: &str = "accept-with-reason";
/// Rollback disposition for an `unmeasurable` table that needs an explicit
/// manual reconciliation step. See `docs/rollback-delta.md` for the semantics.
pub const DISPOSITION_MANUAL_RECONCILE_REQUIRED: &str = "manual-reconcile-required";

/// How a table's recency is measured.
#[derive(Debug, Clone, Copy)]
pub enum TableMeasure {
    /// Timestamp columns that record the row's last write. One column uses a
    /// plain comparison; several use `GREATEST` over null-tolerant casts, so
    /// projection tables (e.g. `members`) catch updates to any column.
    Columns(&'static [&'static str]),
    /// No usable timestamp column: `.0` names the gap (reason) and `.1`
    /// names its rollback handling (`accept-with-reason` or
    /// `manual-reconcile-required`). Tuple form (not a struct variant) so
    /// `matches!(m, TableMeasure::Unmeasurable(..))` keeps compiling.
    Unmeasurable(&'static str, &'static str),
}

/// One bot-owned table and how its Next-window delta is measured.
#[derive(Debug, Clone, Copy)]
pub struct TableSpec {
    pub table: &'static str,
    pub measure: TableMeasure,
}

/// Every table in `sql/database_role_matrix.sql` (`kind = 'table'`) or the
/// backup allowlist ([`two_bot_core::backup::DUMP_TABLES`]), plus the two
/// feed tables the role-matrix assertion does not cover yet. Keep this list
/// exhaustive: the coverage test fails on any matrix/allowlist table missing
/// here, and on any entry here that appears in neither list (outside the two
/// documented feed tables).
pub const TABLE_SPECS: &[TableSpec] = &[
    // Admin API token registry (0456/0457): issuance plus the revocation and
    // last-use lifecycle writes; expires_at is a future deadline, not a
    // write time, so it stays out of the measure.
    TableSpec { table: "admin_api_tokens", measure: TableMeasure::Columns(&["issued_at", "revoked_at", "last_used_at"]) },
    TableSpec { table: "announcements_audit_log", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "assistant_monthly_builds", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "audit_kill_switch", measure: TableMeasure::Columns(&["engaged_at"]) },
    TableSpec { table: "automation_audit_log", measure: TableMeasure::Columns(&["created_at"]) },
    // Pending-publication marker (0468): requested_at is stamped once at the
    // first failed publication and the row is deleted on the next confirmed
    // one, so it is the row's only write time.
    TableSpec { table: "automation_command_registry_resync", measure: TableMeasure::Columns(&["requested_at"]) },
    TableSpec { table: "automation_commands", measure: TableMeasure::Columns(&["updated_at"]) },
    // Flag flips (matched/mutation_started/counted/released) happen inside the
    // claim's lifetime, so a delta claim shows up via claimed_at/completed_at.
    TableSpec { table: "automod_delivery_claims", measure: TableMeasure::Columns(&["claimed_at", "completed_at"]) },
    TableSpec { table: "automod_processed_messages", measure: TableMeasure::Columns(&["processed_at"]) },
    TableSpec { table: "automod_violations", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "backfill_cursors", measure: TableMeasure::Columns(&["updated_at"]) },
    // History rows may be inserted long after their Discord send time during
    // catch-up; sent_at is source time, not the row's write time.
    TableSpec {
        table: "backfill_history_messages",
        measure: TableMeasure::Unmeasurable("sent_at records message time, not when catch-up inserted the row", DISPOSITION_MANUAL_RECONCILE_REQUIRED),
    },
    TableSpec { table: "backfill_history_counts", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "community_facts", measure: TableMeasure::Columns(&["recorded_at"]) },
    TableSpec { table: "community_scorecard_alerts", measure: TableMeasure::Columns(&["created_at"]) },
    // Retry reservation (TOG-11145): next_attempt_at is a BIGINT epoch-ms
    // backoff deadline, not a write time, and has no timestamptz cast.
    TableSpec {
        table: "community_scorecard_attempts",
        measure: TableMeasure::Unmeasurable("no timestamp column; next_attempt_at is an epoch-ms retry deadline, and a completed week is visible via community_scorecard_runs.generated_at", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec { table: "community_scorecard_runs", measure: TableMeasure::Columns(&["generated_at"]) },
    TableSpec { table: "community_stream_heartbeats", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "containment_events", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "containment_incidents", measure: TableMeasure::Columns(&["started_at"]) },
    // Same shape as guild_counters: one read-time per count, no updated_at.
    TableSpec { table: "counter_snapshots", measure: TableMeasure::Columns(&["human_member_count_at", "online_count_at"]) },
    TableSpec { table: "event_rsvps", measure: TableMeasure::Columns(&["responded_at"]) },
    // Append-only event-store capture (0427 voice + 0428 presence + 0429 thread
    // lifecycle + 0430 message/reaction + 0431 stage/interaction/audit + 0432
    // member deltas + 0439 invite/scheduled-event): `event_time` is the gateway event time and `received_at`
    // the capture time (same shape as events.recorded_at), both written once
    // per row; the maximum across both is the row's last write.
    TableSpec { table: "event_store", measure: TableMeasure::Columns(&["event_time", "received_at"]) },
    TableSpec { table: "events", measure: TableMeasure::Columns(&["recorded_at"]) },
    // Event voice idempotency rows (0455): the claim stamps created_at once;
    // bind/clear/orphan transitions rewrite updated_at without touching
    // created_at, so the maximum across both is the row's last write.
    TableSpec { table: "event_voice_channels", measure: TableMeasure::Columns(&["created_at", "updated_at"]) },
    // Event voice presence spans (0455): the join stamps joined_at once; the
    // leave rewrites left_at from NULL without touching joined_at, so the
    // maximum across both is the row's last write (same shape as tickets).
    TableSpec { table: "event_voice_presence", measure: TableMeasure::Columns(&["joined_at", "left_at"]) },
    TableSpec { table: "feed_deliveries", measure: TableMeasure::Columns(&["first_seen_at"]) },
    TableSpec { table: "feed_relays", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "game_activity_guild_aggregates", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "gateway_boot_directives", measure: TableMeasure::Columns(&["armed_at", "consumed_at"]) },
    TableSpec {
        table: "gateway_onboarding_jobs",
        measure: TableMeasure::Unmeasurable("transient restart-recovery queue with no timestamp column (occurred_at_ms is a gateway event time); rows hold no durable member state", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec { table: "gateway_sessions", measure: TableMeasure::Columns(&["updated_at"]) },
    // Timed-entry giveaways (0460/0461): every definition mutation stamps
    // updated_at (create, claim, release, status, draw, announce), so its
    // maximum is the row's last write. Entries and draws are insert-only;
    // their insert stamps are the last write.
    TableSpec { table: "giveaways", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "giveaway_entries", measure: TableMeasure::Columns(&["entered_at"]) },
    TableSpec { table: "giveaway_draws", measure: TableMeasure::Columns(&["drawn_at"]) },
    // Collector read-times ride the same row write, so their maximum is the
    // row's last write.
    TableSpec { table: "guild_analytics_settings", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "guild_counters", measure: TableMeasure::Columns(&["human_member_count_at", "online_count_at"]) },
    TableSpec { table: "guild_settings", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "guild_settings_audit", measure: TableMeasure::Columns(&["at"]) },
    TableSpec {
        table: "guild_settings_revision",
        measure: TableMeasure::Unmeasurable("monotonic revision counter with no timestamp column; changes are visible via guild_settings.updated_at", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec { table: "internal_action_log", measure: TableMeasure::Columns(&["created_at"]) },
    // One row per clock domain, rewritten on every burn; observed_at is the
    // DB instant that advanced the mark, so its maximum is the row's last write.
    TableSpec { table: "internal_clock_high_water", measure: TableMeasure::Columns(&["observed_at"]) },
    TableSpec { table: "internal_discord_events", measure: TableMeasure::Columns(&["claimed_at"]) },
    // Re-mapped keys rewrite updated_at without touching created_at, so the
    // maximum across both is the row's last write (same shape as tickets).
    TableSpec { table: "internal_event_keys", measure: TableMeasure::Columns(&["created_at", "updated_at"]) },
    TableSpec { table: "internal_idempotency", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "internal_nonces", measure: TableMeasure::Columns(&["burned_at"]) },
    // Retirements rewrite disabled_at without touching created_at, so the
    // maximum across both is the row's last write (same shape as tickets).
    TableSpec { table: "invite_campaigns", measure: TableMeasure::Columns(&["created_at", "disabled_at"]) },
    TableSpec { table: "invite_snapshots", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "join_risk_flags", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "level_import_runs", measure: TableMeasure::Columns(&["imported_at"]) },
    TableSpec {
        table: "level_role_rewards",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see replace_role_rewards)", DISPOSITION_MANUAL_RECONCILE_REQUIRED),
    },
    TableSpec {
        table: "lfg_posts",
        measure: TableMeasure::Columns(&["created_at"]),
    },
    TableSpec {
        table: "lfg_roles",
        measure: TableMeasure::Unmeasurable("no timestamp column; child rows are created and updated with the parent lfg_posts row", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec { table: "lfg_signups", measure: TableMeasure::Columns(&["joined_at"]) },
    TableSpec { table: "member_catchup_cursors", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "member_catchup_members", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "member_erasure_audit", measure: TableMeasure::Columns(&["erased_at"]) },
    TableSpec { table: "member_exclusions", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "member_levels", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "member_ranks", measure: TableMeasure::Columns(&["updated_at"]) },
    // Seasonal projection (migrations 0100/0101): award upserts move
    // updated_at and reset the decayed_weeks watermark; decay rewrites
    // seasonal_xp/decayed_weeks but leaves an occurred_at-stamped decay row
    // in xp_awards, mirroring the member_levels lifetime path.
    // Private member reports (0469): evidence and reporter identities that are
    // erased on request and deleted 90 days after closure. Never exported or
    // archived, so a rollback artifact cannot outlive those guarantees. No
    // legacy system reads them, so a rollback leaves them inert.
    TableSpec {
        table: "member_reports",
        measure: TableMeasure::Unmeasurable("private report evidence is never exported or archived (90-day retention and erasure guarantees); no legacy system reads it, so a rollback leaves the tables inert", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec { table: "member_season_xp", measure: TableMeasure::Columns(&["updated_at"]) },
    // Pure projection: any milestone/activity write must move the maximum.
    TableSpec {
        table: "members",
        measure: TableMeasure::Columns(&[
            "joined_at",
            "first_message_at",
            "third_message_at",
            "first_voice_at",
            "last_active_at",
            "left_at",
            "inactive_flagged_at",
            "gate_cleared_at",
        ]),
    },
    TableSpec { table: "moderation_audit", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec {
        table: "moderation_channel_executions",
        measure: TableMeasure::Unmeasurable("no timestamp column; the execution fence row is written with its parent moderation_idempotency claim (measured via claimed_at)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec { table: "moderation_idempotency", measure: TableMeasure::Columns(&["claimed_at"]) },
    TableSpec { table: "moderation_lockdowns", measure: TableMeasure::Columns(&["locked_at"]) },
    TableSpec { table: "moderation_member_bans", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "moderation_scheduled_unbans", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "moderation_warnings", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "operational_audit_log", measure: TableMeasure::Columns(&["occurred_at"]) },
    TableSpec { table: "presence_probe", measure: TableMeasure::Columns(&["observed_at"]) },
    TableSpec { table: "privacy_opt_out_audit", measure: TableMeasure::Columns(&["opted_out_at"]) },
    TableSpec { table: "privacy_opt_outs", measure: TableMeasure::Columns(&["opted_out_at"]) },
    TableSpec {
        table: "rank_ladder",
        measure: TableMeasure::Unmeasurable("no timestamp column; rows are rewritten with each rank snapshot (measured via rank_snapshots.snapshot_at in the same transaction, see WebsiteStore::write_rank_snapshot)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec { table: "rank_snapshots", measure: TableMeasure::Columns(&["snapshot_at"]) },
    TableSpec {
        table: "report_reporters",
        measure: TableMeasure::Unmeasurable("private report evidence is never exported or archived (90-day retention and erasure guarantees); no legacy system reads it, so a rollback leaves the tables inert", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "report_snapshots",
        measure: TableMeasure::Unmeasurable("private report evidence is never exported or archived (90-day retention and erasure guarantees); no legacy system reads it, so a rollback leaves the tables inert", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec { table: "rollback_journal", measure: TableMeasure::Columns(&["recorded_at"]) },
    TableSpec { table: "rollback_watermarks", measure: TableMeasure::Columns(&["updated_at"]) },
    // Retention deletion ledger (0438): request creation stamps requested_at,
    // completion rewrites completed_at without touching requested_at, so the
    // maximum across both is the row's last write (same shape as tickets).
    TableSpec {
        table: "retention_deletion_requests",
        measure: TableMeasure::Columns(&["requested_at", "completed_at"]),
    },
    // Retention guild-purge queue (0438): insert-once per guild, stamped on
    // enqueue; a purge deletes the row, so only enqueues are measurable.
    TableSpec { table: "retention_guild_purge_queue", measure: TableMeasure::Columns(&["enqueued_at"]) },
    TableSpec { table: "scheduled_events", measure: TableMeasure::Columns(&["updated_at"]) },
    // Epoch-claim guard for the scheduled-event mirror (0450/0451): every
    // winning fetch rewrites epoch inside the swap transaction, so its
    // maximum is the row's last write.
    TableSpec { table: "scheduled_events_claim", measure: TableMeasure::Columns(&["epoch"]) },
    // Delete tombstones for the scheduled-event mirror (0452/0453): every
    // delete writes deleted_at and every re-delete refreshes it forward, so
    // its maximum is the row's last write (purges and cap trims only remove
    // rows, like any other deletion).
    TableSpec { table: "scheduled_events_tombstone", measure: TableMeasure::Columns(&["deleted_at"]) },
    TableSpec { table: "scheduled_messages", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "self_role_audit", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec {
        table: "self_role_exchange_baselines",
        measure: TableMeasure::Unmeasurable("no timestamp column; baseline rows are pinned/retired in place by the legacy-journal path without touching self_role_exchanges (see pin_legacy_exchange, retire_exchange_evidence); accepted with a per-event baseline dump comparison as alternate evidence", DISPOSITION_ACCEPT_WITH_REASON),
    },
    // Ticket creation, settlement and retirement each rewrite a different
    // column without touching the others, so the maximum is the row's last write.
    TableSpec {
        table: "self_role_exchanges",
        measure: TableMeasure::Columns(&["created_at", "completed_at", "retired_at"]),
    },
    TableSpec { table: "self_role_panel_claims", measure: TableMeasure::Columns(&["processing_expires_at"]) },
    TableSpec { table: "starboard_config", measure: TableMeasure::Columns(&["updated_at"]) },
    TableSpec { table: "starboard_starrers", measure: TableMeasure::Columns(&["created_at"]) },
    TableSpec { table: "starboard_posts", measure: TableMeasure::Columns(&["created_at", "updated_at"]) },
    TableSpec { table: "sticky_messages", measure: TableMeasure::Columns(&["updated_at"]) },
    // Claim/start/stamp/takeover transitions rewrite claimed_at,
    // send_started_at and delivered_at without touching the others, so the
    // maximum across all three is the row's last write (same shape as
    // tickets). Migrations 0440/0443.
    TableSpec { table: "stream_live_claims", measure: TableMeasure::Columns(&["claimed_at", "send_started_at", "delivered_at"]) },
    TableSpec { table: "ticket_transcripts", measure: TableMeasure::Columns(&["created_at"]) },
    // Status/close transitions rewrite these columns without touching
    // created_at, so the maximum across all three is the row's last write.
    TableSpec {
        table: "tickets",
        measure: TableMeasure::Columns(&["created_at", "closing_started_at", "closed_at"]),
    },
    TableSpec {
        table: "voice_creators",
        measure: TableMeasure::Unmeasurable("no timestamp column; creator configuration is upserted in place (see PgRoomStore::add_creator)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    // V3 block list (0416): one row per blocked member, stamped on insert; an
    // unblock deletes the row, so only additions are measurable.
    TableSpec { table: "voice_room_blocks", measure: TableMeasure::Columns(&["created_at"]) },
    // V3 approved Connect grants (0418): one row per approved member, stamped
    // on intent; a revocation or refused grant deletes the row, so only
    // approvals are measurable.
    TableSpec { table: "voice_join_grants", measure: TableMeasure::Columns(&["created_at"]) },
    // Insert-once creation snapshot plus V2 ownership handoffs, whose
    // timestamp lives in `owner_touched_at` (migration 0412), V3 `/name`
    // custom-name changes, stamped in `name_touched_at` (migration 0414), and V3
    // privacy writes, stamped in `privacy_touched_at` (migration 0416).
    TableSpec {
        table: "voice_rooms",
        measure: TableMeasure::Columns(&["created_at", "owner_touched_at", "name_touched_at", "privacy_touched_at"]),
    },
    // Capture both accepted creates and post-baseline bindings/rollbacks of
    // reservations created before the baseline (migration 0422).
    TableSpec { table: "voice_create_reservations", measure: TableMeasure::Columns(&["created_at", "settled_at"]) },
    TableSpec { table: "voice_owner_grants", measure: TableMeasure::Columns(&["touched_at"]) },
    // Same insert-once shape as voice_rooms (PgRoomStore::add_companion never
    // overwrites the creation snapshot).
    TableSpec { table: "voice_text_companions", measure: TableMeasure::Columns(&["created_at"]) },
    // V11b configuration tables (0229): replaced wholesale by
    // PgVoiceConfigStore::apply, so none carries a write timestamp.
    TableSpec {
        table: "voice_channel_templates",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_game_aliases",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_random_lists",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_random_list_choices",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_logging",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_logging_mention_members",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_logging_mention_roles",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_guild_settings",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_command_roles",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_command_role_members",
        measure: TableMeasure::Unmeasurable("no timestamp column; configuration is replaced wholesale (see PgVoiceConfigStore::apply)", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_logging_settings",
        measure: TableMeasure::Unmeasurable("mutable per-guild settings row with no timestamp column; no member IDs", DISPOSITION_ACCEPT_WITH_REASON),
    },
    TableSpec {
        table: "voice_access_controls",
        measure: TableMeasure::Unmeasurable("mutable per-guild settings row with no timestamp column; no member IDs", DISPOSITION_ACCEPT_WITH_REASON),
    },
    // Append-only vote-kick audit rows (migration 0417); `occurred_at` is the
    // event time, written once per row.
    TableSpec { table: "voice_vote_kick_audit", measure: TableMeasure::Columns(&["occurred_at"]) },
    TableSpec {
        table: "web_contract_meta",
        measure: TableMeasure::Unmeasurable("singleton contract row with no timestamp column", DISPOSITION_MANUAL_RECONCILE_REQUIRED),
    },
    TableSpec { table: "xp_awards", measure: TableMeasure::Columns(&["occurred_at"]) },
    TableSpec { table: "xp_cooldowns", measure: TableMeasure::Columns(&["last_awarded_at"]) },
];

/// Tables the exporter emits only as parents of exported rows, never via a
/// timestamp predicate. `lfg_roles` has no timestamp column, but an exported
/// post-freeze `lfg_posts` row is unusable without its roles: `lfg_signups`
/// references `(lfg_id, role_key)`, so the signup insert aborts the whole
/// transaction on the missing parent. [`export_delta`] therefore emits the
/// roles of every exported post alongside the measured rows; [`crate::rollback_import`]
/// accepts these tables even though [`report`] still lists them as
/// `unmeasurable` (no timestamp column to measure).
pub const PARENT_JOINED_TABLES: &[&str] = &["lfg_roles"];

/// Feed tables predate the role-matrix assertion; they are measured here and
/// the coverage test names them explicitly so a future matrix addition cannot
/// hide behind this list.
pub const EXTRA_MEASURED_TABLES: &[&str] = &["feed_relays", "feed_deliveries"];

#[derive(Debug, Error)]
pub enum DeltaError {
    #[error("database error (details omitted)")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Usage(String),
    #[error("export row cap exceeded for table {0}")]
    ExportCap(String),
    #[error("cannot write export file")]
    ExportIo(#[from] std::io::Error),
}

/// `--since` validation failure.
#[derive(Debug, PartialEq, Eq)]
pub enum SinceError {
    Missing,
    Invalid,
    Future,
}

impl std::fmt::Display for SinceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinceError::Missing => write!(f, "--since <RFC3339> is required"),
            SinceError::Invalid => write!(f, "--since must be RFC3339 (e.g. 2026-09-30T12:00:00Z)"),
            SinceError::Future => write!(f, "--since must not be in the future"),
        }
    }
}

/// Validate the `T_f` baseline: present, RFC3339, not in the future.
pub fn parse_since(raw: Option<&str>) -> Result<time::OffsetDateTime, SinceError> {
    let raw = raw
        .filter(|s| !s.trim().is_empty())
        .ok_or(SinceError::Missing)?;
    let since = time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map_err(|_| SinceError::Invalid)?;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let now = time::OffsetDateTime::from_unix_timestamp(now_secs)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    if since > now {
        return Err(SinceError::Future);
    }
    Ok(since)
}

/// Quote a static identifier. Table/column names are compile-time constants,
/// so this only ever fails on a programming error; still, never interpolate
/// an unquoted name (`at` is a reserved word).
fn quote(name: &str) -> Result<String, DeltaError> {
    if name.len() > 63
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || !name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
    {
        return Err(DeltaError::Usage(format!("invalid identifier: {name}")));
    }
    Ok(format!("\"{name}\""))
}

/// `recency > T_f` for one spec's columns. Single column: plain comparison.
/// Several: `GREATEST` over null-tolerant casts so a NULL column never sinks
/// the row (`GREATEST` returns NULL if any argument is NULL, hence `COALESCE`
/// to `-infinity`).
fn recency_predicate(spec: &TableSpec) -> Result<String, DeltaError> {
    let TableMeasure::Columns(columns) = spec.measure else {
        return Err(DeltaError::Usage(format!(
            "table {} is unmeasurable",
            spec.table
        )));
    };
    if columns.is_empty() {
        return Err(DeltaError::Usage(format!(
            "table {} has no columns",
            spec.table
        )));
    }
    let cast = |column: &str| -> Result<String, DeltaError> {
        Ok(format!("{}::timestamptz", quote(column)?))
    };
    let expr = if columns.len() == 1 {
        cast(columns[0])?
    } else {
        let arms = columns
            .iter()
            .map(|c| Ok(format!("COALESCE({}, '-infinity'::timestamptz)", cast(c)?)))
            .collect::<Result<Vec<_>, DeltaError>>()?
            .join(", ");
        format!("GREATEST({arms})")
    };
    Ok(format!("{expr} > $1::timestamptz"))
}

/// Per-table delta entry in the JSON summary.
#[derive(Debug, Serialize)]
pub struct TableDelta {
    pub table: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Present only on `unmeasurable` entries: `accept-with-reason` or
    /// `manual-reconcile-required`. Absent (not null) on `measured` and
    /// `missing` entries so those entries stay byte-identical to before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
}

/// The JSON summary: one entry per spec table, never silently skipped.
#[derive(Debug, Serialize)]
pub struct DeltaReport {
    pub version: u32,
    pub since: String,
    pub generated_at: String,
    pub tables: Vec<TableDelta>,
}

/// Count one measured table inside the caller's read-only transaction. A
/// missing table (partially migrated database) is reported, not an error.
async fn count_table(
    tx: &mut Transaction<'_, Postgres>,
    spec: &TableSpec,
    since: &str,
) -> Result<TableDelta, DeltaError> {
    let table = quote(spec.table)?;
    // Unqualified on purpose: it resolves through the same search_path as the
    // COUNT below, so schema-isolated test databases behave like production.
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(spec.table)
        .fetch_one(&mut **tx)
        .await?;
    if !exists {
        return Ok(TableDelta {
            table: spec.table.to_owned(),
            status: "missing".to_owned(),
            columns: None,
            count: None,
            reason: Some("table not present in this database".to_owned()),
            disposition: None,
        });
    }
    let predicate = recency_predicate(spec)?;
    // Table and column names are static constants validated by `quote`;
    // only the timestamp bound is bound.
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {predicate}");
    let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(since)
        .fetch_one(&mut **tx)
        .await?;
    Ok(TableDelta {
        table: spec.table.to_owned(),
        status: "measured".to_owned(),
        columns: match spec.measure {
            TableMeasure::Columns(columns) => {
                Some(columns.iter().map(|c| (*c).to_owned()).collect())
            }
            TableMeasure::Unmeasurable(..) => None,
        },
        count: Some(count),
        reason: None,
        disposition: None,
    })
}

/// Build the full summary inside one read-only snapshot transaction.
pub async fn report(pool: &PgPool, since: &str) -> Result<DeltaReport, DeltaError> {
    report_at(pool, since, crate::cli::now_iso()).await
}

async fn report_at(
    pool: &PgPool,
    since: &str,
    generated_at: String,
) -> Result<DeltaReport, DeltaError> {
    let mut conn = pool.acquire().await?;
    let mut tx = crate::legacy_verify::read_only_transaction(&mut conn)
        .await
        .map_err(|e| match e {
            crate::legacy_verify::VerifyError::Database(inner) => DeltaError::Database(inner),
            other => DeltaError::Usage(other.to_string()),
        })?;
    let mut tables = Vec::with_capacity(TABLE_SPECS.len());
    for spec in TABLE_SPECS {
        match spec.measure {
            TableMeasure::Unmeasurable(reason, disposition) => tables.push(TableDelta {
                table: spec.table.to_owned(),
                status: "unmeasurable".to_owned(),
                columns: None,
                count: None,
                reason: Some(reason.to_owned()),
                disposition: Some(disposition.to_owned()),
            }),
            TableMeasure::Columns(_) => tables.push(count_table(&mut tx, spec, since).await?),
        }
    }
    tx.rollback().await?;
    Ok(DeltaReport {
        version: REPORT_VERSION,
        since: since.to_owned(),
        generated_at,
        tables,
    })
}

/// Export every measured post-`T_f` row as NDJSON (`{"table": ..., "row": ...}`
/// per line) into the caller's read-only snapshot, plus the [`PARENT_JOINED_TABLES`]
/// rows of every exported `lfg_posts` row (parent join, not a timestamp
/// predicate). Refuses when any table exceeds [`MAX_EXPORT_ROWS_PER_TABLE`]
/// rather than silently truncating.
pub async fn export_delta(
    pool: &PgPool,
    since: &str,
    writer: &mut dyn FnMut(String) -> std::io::Result<()>,
) -> Result<u64, DeltaError> {
    let mut conn = pool.acquire().await?;
    let mut tx = crate::legacy_verify::read_only_transaction(&mut conn)
        .await
        .map_err(|e| match e {
            crate::legacy_verify::VerifyError::Database(inner) => DeltaError::Database(inner),
            other => DeltaError::Usage(other.to_string()),
        })?;
    let mut exported: u64 = 0;
    let mut exported_post_ids: Vec<String> = Vec::new();
    for spec in TABLE_SPECS {
        let TableMeasure::Columns(_) = spec.measure else {
            continue;
        };
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(spec.table)
            .fetch_one(&mut *tx)
            .await?;
        if !exists {
            continue;
        }
        let predicate = recency_predicate(spec)?;
        let table = quote(spec.table)?;
        let count_sql = format!("SELECT COUNT(*) FROM {table} WHERE {predicate}");
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(count_sql))
            .bind(since)
            .fetch_one(&mut *tx)
            .await?;
        if count as u64 > MAX_EXPORT_ROWS_PER_TABLE {
            return Err(DeltaError::ExportCap(spec.table.to_owned()));
        }
        if count == 0 {
            continue;
        }
        let row_sql = format!("SELECT to_jsonb(t.*)::text FROM {table} t WHERE {predicate}");
        let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(row_sql))
            .bind(since)
            .fetch_all(&mut *tx)
            .await?;
        for row in rows {
            let value: serde_json::Value =
                serde_json::from_str(&row).unwrap_or(serde_json::Value::Null);
            if spec.table == "lfg_posts" {
                if let Some(id) = value.get("id").and_then(serde_json::Value::as_str) {
                    exported_post_ids.push(id.to_owned());
                }
            }
            let line = serde_json::json!({"table": spec.table, "row": value}).to_string();
            writer(line)?;
            exported += 1;
        }
    }
    // Parent join: the roles of every exported post round-trip with the post
    // and its signups, so the `(lfg_id, role_key)` FK never aborts the import
    // on a missing parent. Roles carry no timestamp, hence no recency
    // predicate: membership in this set is by exported post id.
    if !exported_post_ids.is_empty() {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass('lfg_roles') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
        if exists {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM lfg_roles WHERE lfg_id = ANY($1)")
                    .bind(&exported_post_ids)
                    .fetch_one(&mut *tx)
                    .await?;
            if count as u64 > MAX_EXPORT_ROWS_PER_TABLE {
                return Err(DeltaError::ExportCap("lfg_roles".to_owned()));
            }
            if count > 0 {
                let rows: Vec<String> = sqlx::query_scalar(
                    "SELECT to_jsonb(t.*)::text FROM lfg_roles t WHERE lfg_id = ANY($1)",
                )
                .bind(&exported_post_ids)
                .fetch_all(&mut *tx)
                .await?;
                for row in rows {
                    let value: serde_json::Value =
                        serde_json::from_str(&row).unwrap_or(serde_json::Value::Null);
                    let line = serde_json::json!({"table": "lfg_roles", "row": value}).to_string();
                    writer(line)?;
                    exported += 1;
                }
            }
        }
    }
    tx.rollback().await?;
    Ok(exported)
}

/// File-writing sink for [`export_delta`].
pub fn file_writer(file: std::fs::File) -> impl FnMut(String) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = file;
    move |line: String| {
        writeln!(file, "{line}")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const MATRIX_SQL: &str = include_str!("../../../sql/database_role_matrix.sql");

    fn matrix_tables() -> BTreeSet<String> {
        MATRIX_SQL
            .lines()
            .filter(|l| l.trim_end().ends_with("'table'),"))
            .filter_map(|l| {
                let mut parts = l.split('\'');
                parts.next()?;
                let schema = parts.next()?;
                parts.next()?;
                let name = parts.next()?;
                (schema == "public").then(|| name.to_owned())
            })
            .collect()
    }

    #[test]
    fn since_requires_present_rfc3339_past() {
        assert_eq!(parse_since(None), Err(SinceError::Missing));
        assert_eq!(parse_since(Some("")), Err(SinceError::Missing));
        assert_eq!(parse_since(Some("  ")), Err(SinceError::Missing));
        assert_eq!(parse_since(Some("yesterday")), Err(SinceError::Invalid));
        assert_eq!(
            parse_since(Some("2026-13-01T00:00:00Z")),
            Err(SinceError::Invalid)
        );
        assert_eq!(
            parse_since(Some("2999-01-01T00:00:00Z")),
            Err(SinceError::Future)
        );
        assert!(parse_since(Some("2026-09-30T12:00:00Z")).is_ok());
        assert!(parse_since(Some("2026-09-30T12:00:00+00:00")).is_ok());
    }

    #[test]
    fn every_matrix_and_allowlist_table_is_classified() {
        let specs: BTreeSet<String> = TABLE_SPECS.iter().map(|s| s.table.to_owned()).collect();
        // No duplicate specs.
        assert_eq!(specs.len(), TABLE_SPECS.len());
        let mut universe = matrix_tables();
        for table in two_bot_core::backup::DUMP_TABLES {
            universe.insert((*table).to_owned());
        }
        for table in EXTRA_MEASURED_TABLES {
            universe.insert((*table).to_owned());
        }
        let unclassified: Vec<_> = universe.difference(&specs).collect();
        assert!(
            unclassified.is_empty(),
            "unclassified tables would be silently skipped: {unclassified:?}"
        );
        // Nothing extra beyond the documented feed tables: a new measured
        // table must arrive via the matrix or the allowlist.
        let mut allowed_extra: BTreeSet<String> = EXTRA_MEASURED_TABLES
            .iter()
            .map(|t| (*t).to_owned())
            .collect();
        allowed_extra.extend(universe);
        let unexpected: Vec<_> = specs.difference(&allowed_extra).collect();
        assert!(unexpected.is_empty(), "unexpected specs: {unexpected:?}");
    }

    #[test]
    fn recency_predicates_cast_and_guard_nulls() {
        let single = TableSpec {
            table: "events",
            measure: TableMeasure::Columns(&["recorded_at"]),
        };
        assert_eq!(
            recency_predicate(&single).unwrap(),
            "\"recorded_at\"::timestamptz > $1::timestamptz"
        );
        let multi = TableSpec {
            table: "members",
            measure: TableMeasure::Columns(&["joined_at", "left_at"]),
        };
        assert_eq!(
            recency_predicate(&multi).unwrap(),
            "GREATEST(COALESCE(\"joined_at\"::timestamptz, '-infinity'::timestamptz), COALESCE(\"left_at\"::timestamptz, '-infinity'::timestamptz)) > $1::timestamptz"
        );
        assert!(recency_predicate(&TableSpec {
            table: "rank_ladder",
            measure: TableMeasure::Unmeasurable("seed data", DISPOSITION_ACCEPT_WITH_REASON),
        })
        .is_err());
        assert!(quote("guild_settings; DROP TABLE x").is_err());
        assert!(quote("at").is_ok());
    }

    #[test]
    fn every_unmeasurable_table_carries_a_known_disposition() {
        let mut saw_accept = false;
        let mut saw_manual = false;
        for spec in TABLE_SPECS {
            let TableMeasure::Unmeasurable(reason, disposition) = spec.measure else {
                continue;
            };
            assert!(
                !reason.is_empty(),
                "table {} has an empty reason",
                spec.table
            );
            assert!(
                [
                    DISPOSITION_ACCEPT_WITH_REASON,
                    DISPOSITION_MANUAL_RECONCILE_REQUIRED
                ]
                .contains(&disposition),
                "table {} has unknown disposition {disposition}",
                spec.table
            );
            saw_accept |= disposition == DISPOSITION_ACCEPT_WITH_REASON;
            saw_manual |= disposition == DISPOSITION_MANUAL_RECONCILE_REQUIRED;
        }
        assert!(saw_accept, "lost every accept-with-reason table");
        assert!(saw_manual, "lost every manual-reconcile-required table");
    }
}
