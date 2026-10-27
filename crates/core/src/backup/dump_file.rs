//! The v15 backup envelope (also reads v3/v4/v5/v6/v7/v8/v9/v10/v11/v12/v13/v14): manifest / rows / end marker, gzipped NDJSON.
//!
//! Port of the file half of legacy `src/store/dump.ts`. The writer side lives
//! in [`super::dump`]; everything here touches no database, so every refusal
//! below is pinned by a test that runs without Postgres.
//!
//! ## The format
//!
//! Gzipped NDJSON, one JSON object per line, in three kinds:
//!
//! ```text
//! {"kind":"manifest", version, createdAt, tables:[{name,columns,columnTypes,count}], ...}
//! {"kind":"row", table, data:{...}}
//! {"kind":"end", rows}
//! ```
//!
//! Values are stored in Postgres text-output form (`SELECT col::text`, one
//! `Option<String>` per cell) with the column type recorded per table in the
//! manifest. Restore re-applies the value with a `$n::type` cast, so the
//! round trip is faithful for every type the bot owns (text, integers,
//! timestamptz, booleans, json/jsonb, bytea hex, arrays, enums, intervals)
//! without per-type Rust decoding. `column_types` is additive to legacy v3.
//! Frozen legacy dumps omit it and contain native numbers/booleans; restore
//! converts these to PostgreSQL input text using the target's column types.
//! JSON-looking TEXT stays unchanged; native JSON cells are serialized only
//! for a JSON/JSONB target. Unsupported composite cells refuse, never become NULL.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Read, Write};
use std::path::Path;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// Durable bot-owned tables, parents before children. The first 22 names are
/// frozen v3 coverage; append new tables after that prefix. Tables retired from
/// the Rust migration set remain supported when present on a legacy target.
/// The bot-owned website-contract backing tables ARE included; derived views
/// and the website service's own database are not application data here.
pub const DUMP_TABLES: &[&str] = &[
    "events",
    "members",
    "invite_snapshots",
    "operational_audit_log",
    "moderation_warnings",
    "moderation_scheduled_unbans",
    "moderation_audit",
    "moderation_lockdowns",
    "moderation_idempotency",
    "containment_events",
    "containment_incidents",
    "join_risk_flags",
    "automation_commands",
    "scheduled_messages",
    "sticky_messages",
    "automation_audit_log",
    "tickets",
    "ticket_transcripts",
    "automod_violations",
    "automod_processed_messages",
    "self_role_audit",
    "self_role_panel_claims",
    "self_role_exchanges",
    "self_role_exchange_baselines",
    // Appended after the frozen v3 prefix: per-delivery automod arbitration
    // (0221 + counted/preserved-match columns in 0222/0223), one-shot gateway
    // boot directives (0321) and the erasure accountability log (0410). None
    // carries foreign keys, so order among them is free.
    "automod_delivery_claims",
    "gateway_boot_directives",
    "member_erasure_audit",
    // Opt-out control plane (0433/0434): suppression tombstones must survive
    // backup/restore, or a restore would silently resume per-user persistence
    // members refused. No foreign keys, so order is free.
    "privacy_opt_outs",
    "guild_analytics_settings",
    "privacy_opt_out_audit",
    // Game-activity guild aggregates (0441/0442): guild-keyed counters with
    // no member column, archived like the other aggregate tables. No foreign
    // keys, so order is free.
    "game_activity_guild_aggregates",
    // Assistant monthly-cap ledger (0446/0447): one guild-keyed counter row
    // per guild with no member column, archived so caps survive a restore
    // fail-closed (an excluded ledger would grant a fresh quota after every
    // restore). No foreign keys, so order is free.
    "assistant_monthly_builds",
    "member_levels",
    // Seasonal XP projection (0100): append-only mirror of lifetime XP per
    // quarter, guild-keyed with no foreign keys, so order beside
    // member_levels is free.
    "member_season_xp",
    "xp_awards",
    "level_role_rewards",
    "level_import_runs",
    "event_rsvps",
    "announcements_audit_log",
    "community_facts",
    "lfg_posts",
    "lfg_roles",
    "lfg_signups",
    "feed_relays",
    "feed_deliveries",
    "web_contract_meta",
    "guild_counters",
    "rank_ladder",
    "rank_snapshots",
    "member_ranks",
    "scheduled_events",
    // Scheduled-event mirror epoch-claim guard (0450/0451, v10): one row per
    // guild, rewritten on every winning fetch. Archived so a restore stays
    // fail-closed (an excluded guard would grant a fresh epoch after every
    // restore and let a stale in-flight fetch resurrect rows); see
    // [`V10_ADDED_TABLES`]. No foreign keys, so order beside the mirror is free.
    "scheduled_events_claim",
    // Scheduled-event mirror delete tombstones (0452/0453, v11): one row per
    // removed id, suppressing equal-stamp stale replays. Archived so a
    // restore stays fail-closed (an excluded tombstone set would let a stale
    // in-flight fetch resurrect rows); see [`V11_ADDED_TABLES`]. No foreign
    // keys, so order beside the mirror is free.
    "scheduled_events_tombstone",
    // Event voice channels (0455, v13): the per-instance idempotency row plus
    // the per-member presence spans behind after-party ownership. Archived so
    // a restore keeps the claim (an excluded row would let a second worker
    // create a duplicate channel) and the attendance spans; see
    // [`V13_ADDED_TABLES`]. No foreign keys, so order beside the mirror is free.
    "event_voice_channels",
    "event_voice_presence",
    "counter_snapshots",
    "member_exclusions",
    "presence_probe",
    "community_stream_heartbeats",
    "community_scorecard_runs",
    "community_scorecard_alerts",
    "community_scorecard_attempts",
    "gateway_sessions",
    "guild_settings_revision",
    "guild_settings",
    "guild_settings_audit",
    // Admin API token registry (0456/0457, v12): operator-issued tokens with
    // hash-only secrets. Archived so a restore keeps admin API access working
    // (secrets are shown once at issuance and cannot be re-derived); see
    // [`V12_ADDED_TABLES`]. No foreign keys, so order here is free.
    "admin_api_tokens",
    "audit_kill_switch",
    "internal_nonces",
    "internal_clock_high_water",
    "internal_idempotency",
    "internal_action_log",
    "internal_discord_events",
    "internal_event_keys",
    // Stream go-live dedup ledger (0440/0443, v8): one row per go-live, no
    // foreign keys, so order among the internal tables is free; see
    // [`V8_ADDED_TABLES`].
    "stream_live_claims",
    "moderation_channel_executions",
    "moderation_member_bans",
    "invite_campaigns",
    "voice_creators",
    "voice_rooms",
    // V3 block list: references voice_rooms, so it restores after it.
    "voice_room_blocks",
    // V3 approved Connect grants (0418): references voice_rooms, so it
    // restores after it, beside the block list it complements.
    "voice_join_grants",
    "voice_owner_grants",
    "voice_text_companions",
    "voice_access_controls",
    "voice_logging_settings",
    // V11b configuration tables (0229). Guild-keyed, no foreign keys, so order
    // among them is free.
    "voice_channel_templates",
    "voice_game_aliases",
    "voice_random_lists",
    "voice_random_list_choices",
    "voice_logging",
    "voice_logging_mention_members",
    "voice_logging_mention_roles",
    "voice_guild_settings",
    "voice_command_roles",
    "voice_command_role_members",
    // V4 vote-kick audit trail (0417): append-only, guild-keyed, no foreign keys.
    "voice_vote_kick_audit",
    // V5 adds accepted-create history; see [`V5_ADDED_TABLES`]. The seasonal
    // XP projection beside `member_levels` is a v7 addition; see
    // [`V7_ADDED_TABLES`].
    "voice_create_reservations",
    // V6 adds the shared event-store table for the S1-S8 slices (0427 S4
    // voice capture plus 0428 S3 presence plus 0429 S6 thread lifecycle plus
    // 0430 S5 message/reaction capture plus 0431 S8
    // stage/interaction-observe/audit-log capture plus 0432 S2 member
    // capture plus 0439 S7 invite/scheduled-event capture); see
    // [`V6_ADDED_TABLES`]. V7 adds the retention queues (0438)
    // alongside the seasonal XP projection; see [`V7_ADDED_TABLES`]. The
    // retention queues are guild-purge intent and user-deletion ledger with
    // receipts: both must survive restore (a lost purge resurrects departed
    // guild data; a lost request breaches the deletion SLA and drops
    // receipts), so they are dumped, not excluded. Event-store capture is
    // append-only gateway deltas, guild-keyed. No foreign keys on any of
    // them, so order among the appended tables is free.
    "retention_guild_purge_queue",
    "retention_deletion_requests",
    "event_store",
    // V14 starboard history and configuration (0458/0459), no foreign keys.
    "starboard_config",
    "starboard_starrers",
    "starboard_posts",
    // V15 giveaways: parent before entries and immutable draw rounds (0460/0461).
    "giveaways",
    "giveaway_entries",
    "giveaway_draws",
];

/// Tables first covered by v5. A v4 archive predates them and need not declare
/// them; every other covered table is still required, so a v4 recovery point
/// written before create-reservation persistence (0422) stays readable
/// without waiving arbitrary missing tables. The privacy opt-out control plane
/// (0433/0434) is v5 for the same reason: v4 archives predate suppression
/// tombstones, toggle rows and opt-out receipts.
pub const V5_ADDED_TABLES: &[&str] = &[
    "voice_create_reservations",
    "privacy_opt_outs",
    "guild_analytics_settings",
    "privacy_opt_out_audit",
];

/// Tables first covered by v6 (event-store capture: 0427 voice, 0428
/// presence, 0429 thread lifecycle, 0430 message/reaction capture, 0431
/// stage/interaction/audit capture, 0432 member deltas, 0439
/// invite/scheduled-event capture).
/// A v4 or v5 archive predates them and need not declare them; every other
/// covered table is still required, so recovery points written before any of
/// those persistences stay readable without waiving arbitrary missing tables.
/// Restore clears these tables when restoring an older archive, as with other
/// tables absent from an older version.
pub const V6_ADDED_TABLES: &[&str] = &["event_store"];

/// Tables first covered by v7 (seasonal XP projection 0100 plus retention
/// queues, migration 0438). A v4, v5 or v6 archive predates them and need
/// not declare them; v7 archives must cover the full inventory, so recovery
/// points written before the seasonal projection or the retention queues stay
/// readable without waiving arbitrary missing tables. v6 archives written on
/// main after the event-store bump carry no retention tables and must stay
/// readable, hence the retention queues are v7, not v6.
pub const V7_ADDED_TABLES: &[&str] = &[
    "member_season_xp",
    "retention_guild_purge_queue",
    "retention_deletion_requests",
];

/// Tables first covered by v8 (stream go-live dedup ledger, migrations 0440/0443).
/// A v7 or older archive predates claim rows and need not declare them; v8
/// archives must cover the full inventory, so recovery points written before
/// the ledger stay readable without waiving arbitrary missing tables. Restore
/// clears this table when restoring an older archive, as with other tables
/// absent from an older version.
pub const V8_ADDED_TABLES: &[&str] = &["stream_live_claims"];

/// Tables first covered by v9: guild game-activity aggregates (0441/0442) and
/// the assistant monthly-cap ledger (0446/0447). Older archives may omit them;
/// restore clears the destination rows, as for every versioned addition.
/// New archives must contain both counters so their state survives recovery.
pub const V9_ADDED_TABLES: &[&str] =
    &["game_activity_guild_aggregates", "assistant_monthly_builds"];

/// Tables first covered by v10 (scheduled-event mirror epoch-claim guard,
/// migrations 0450/0451). Older archives may omit the guard; restore clears
/// the destination rows, as for every versioned addition. New archives must
/// contain the guard so a restored mirror keeps its stale-fetch fence.
pub const V10_ADDED_TABLES: &[&str] = &["scheduled_events_claim"];

/// Tables first covered by v11 (scheduled-event mirror delete tombstones,
/// migrations 0452/0453). Older archives may omit the tombstones; restore
/// clears the destination rows, as for every versioned addition. New archives
/// must contain the tombstones so a restored mirror keeps its equal-stamp
/// resurrection fence. v10 is a readable legacy shape: claim-only archives
/// written by the merged v10 writer stay readable and clear the tombstones.
pub const V11_ADDED_TABLES: &[&str] = &["scheduled_events_tombstone"];

/// Tables first covered by v12 (admin API token registry, migrations
/// 0456/0457). Older archives may omit the registry; restore clears the
/// destination rows, as for every versioned addition. New archives must
/// contain the registry so issued tokens survive recovery. v11 is a readable
/// legacy shape: tombstone-only archives written by the merged v11 writer
/// stay readable and clear the registry.
pub const V12_ADDED_TABLES: &[&str] = &["admin_api_tokens"];

/// Tables first covered by v13 (event voice channels + presence spans,
/// migration 0455). Older archives may omit both; restore clears the
/// destination rows, as for every versioned addition. New archives must
/// contain both so a restored worker keeps its create claims and the
/// attendance spans behind after-party ownership. v12 keeps its token
/// registry requirement and waives only this new pair.
pub const V13_ADDED_TABLES: &[&str] = &["event_voice_channels", "event_voice_presence"];

/// Tables first covered by v14: the starboard foundation (0458/0459).
/// Older archives may omit them; restore clears their destination rows.
/// The merged v13 writer covered event voice state but not starboard, so
/// v13 retains its frozen inventory and remains readable without these tables.
pub const V14_ADDED_TABLES: &[&str] =
    &["starboard_config", "starboard_starrers", "starboard_posts"];

/// Tables first covered by v15 (timed-entry giveaways, migrations 0460/0461).
/// Released v14 archives retain the starboard, event voice and token registry
/// requirements but may omit giveaways. Restoring them clears destination-only
/// giveaway definitions, entries and draw rounds.
pub const V15_ADDED_TABLES: &[&str] = &["giveaways", "giveaway_entries", "giveaway_draws"];

/// Covered tables not created by the current cutover migration set, if any.
/// Keep legacy data when such tables exist, but do not require nonexistent
/// legacy subsystems. A table created by migrations must leave this list
/// (the coverage test enforces it): empty means every covered table is
/// migrated here, including automation_commands (0130).
pub const OPTIONAL_LEGACY_TABLES: &[&str] = &[];

/// Explicit migrated-schema exclusions, checked by the schema coverage test.
pub const EXCLUDED_TABLES: &[&str] = &[
    // Short-lived XP award throttles, not XP totals/history. Never replay a
    // pre-restore cooldown into a recovered process.
    "xp_cooldowns",
    // Durable send admission is per-credential runtime lane state: occupancy
    // generations and Discord cooldown timing, not application data. Never
    // replay a pre-restore lane hold into a recovered process; a restored
    // database re-admits from generation zero and re-learns cooldowns.
    "discord_send_admission",
    // Onboarding restart-recovery queue: captured member state and delivery
    // receipts bound to a gateway session. Never replay a pre-restore job
    // into a recovered process; welcome/goodbye state of record lives in the
    // onboarding stores, which are archived.
    "gateway_onboarding_jobs",
    // Durable cutover rollback journal and per-table watermarks (store chain
    // 0406). The journal is append-only cutover evidence: the PgFunnelStore
    // funnel append and the production gateway batch writer journal each
    // events insert inside the primary transaction under a savepoint
    // best-effort via journal::record_in, and rows stay re-capturable
    // during the watch window.
    // Never replay pre-restore cutover cursors into a recovered process: a
    // restored watermark could mark post-backup writes as already journaled
    // and silently break rollback coverage.
    "rollback_journal",
    "rollback_watermarks",
    // Resumable Discord backfill progress (store 0408/0409, cutover 0433
    // mirror, cutover 0434 member catch-up): per-family pagination cursors,
    // channel-history metadata catch-up, per-guild member rows and resume
    // cursors. Rows are re-capturable from the Discord API on the next worker
    // run (the cutover ledger marks 0408/0433/0434 re-runnable: drop and
    // re-checkpoint). Never replay a pre-restore cursor into a recovered
    // process: a restored cursor could mark post-backup messages as already
    // backfilled and silently skip them, the same watermark hazard as
    // rollback_watermarks above.
    "backfill_cursors",
    "backfill_history_messages",
    "backfill_history_counts",
    "member_catchup_cursors",
    "member_catchup_members",
    // Private member reports (cutover 0469): bounded single-message evidence
    // and reporter identities that live at most 90 days after a report closes
    // and are redacted or erased on request. A backup would outlive that
    // window and a restore would resurrect expired or erased evidence, so the
    // tables are never archived; the accountability tombstone survives only
    // in the live database.
    "member_reports",
    "report_snapshots",
    "report_reporters",
    // Migration ledgers describe target DDL; replacing them would falsely mark
    // unapplied migrations as applied. Legacy schema_migrations is diagnostic
    // manifest metadata only, never restored application data. The store
    // chain's own runner bookkeeping belongs beside cutover's ledger.
    "_sqlx_migrations",
    "schema_migrations",
    "_two_bot_migrations",
];

/// Columns the destination allocates itself: never archived, never restored.
pub const DESTINATION_OWNED_COLUMNS: &[(&str, &str)] = &[
    // CAS tokens come from the never-reseeded guild_settings_cas_seq, and
    // trg_guild_settings_version replaces supplied tokens on every write. A
    // restore therefore allocates fresh tokens that invalidate every token
    // issued before it, as legacy copy does; archiving them would only record
    // values that restore cannot and must not reproduce.
    ("guild_settings", "cas_version"),
];

/// True when `table.column` is allocated by the destination, see
/// [`DESTINATION_OWNED_COLUMNS`].
#[must_use]
pub fn is_destination_owned(table: &str, column: &str) -> bool {
    DESTINATION_OWNED_COLUMNS
        .iter()
        .any(|(owned_table, owned_column)| *owned_table == table && *owned_column == column)
}

/// Write complete v15 coverage; the reader also accepts frozen v3/v4/v5/v6/v7/v8/v9/v10/v11/v12/v13/v14 coverage.
pub const DUMP_VERSION: u32 = 15;

/// A table name in the dump, validated against [`DUMP_TABLES`].
pub type DumpTable = String;

/// Per-table manifest entry: live column order, Postgres type per column
/// (`format_type()` spelling, used as the restore cast), and the row count
/// taken inside the same snapshot as the rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpTableInfo {
    pub name: String,
    pub columns: Vec<String>,
    /// Postgres type per column, parallel to `columns`. Absent on dumps
    /// written by legacy `two-bot`; restore then casts as `text`.
    #[serde(default)]
    pub column_types: Vec<String>,
    pub count: u64,
}

/// The leading manifest line of a dump.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpManifest {
    pub kind: String,
    pub version: u32,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    pub tables: Vec<DumpTableInfo>,
    /// So a restore can put the id sequence back where it belongs.
    #[serde(rename = "eventsSequence")]
    pub events_sequence: i64,
    /// Which migrations the source had applied, for diagnosing an old backup.
    #[serde(rename = "schemaMigrations")]
    pub schema_migrations: Vec<String>,
    /// Source allocator positions. Absent from v3 and from v4 archives written
    /// before marks existed; restore then knows only the restored rows and the
    /// target's own allocators, which cannot see deleted top rows.
    #[serde(
        rename = "sequenceMarks",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub sequence_marks: Vec<SequenceMark>,
}

/// A sequence's position when the dump was taken, keyed by the column it
/// feeds. Sequences are not MVCC, so the dump reads the live position: it is
/// at or beyond every value in the archived snapshot, never behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceMark {
    pub table: String,
    pub column: String,
    #[serde(rename = "lastValue")]
    pub last_value: i64,
    #[serde(rename = "isCalled")]
    pub is_called: bool,
    pub increment: i64,
}

impl SequenceMark {
    /// The value the source would have allocated next; `None` past `i64`.
    #[must_use]
    pub fn next(&self) -> Option<i64> {
        if self.is_called {
            self.last_value.checked_add(self.increment)
        } else {
            Some(self.last_value)
        }
    }
}

impl DumpManifest {
    /// The archived position of the allocator feeding `table.column`, if any.
    #[must_use]
    pub fn sequence_mark(&self, table: &str, column: &str) -> Option<&SequenceMark> {
        self.sequence_marks
            .iter()
            .find(|mark| mark.table == table && mark.column == column)
    }

    /// Old v3 archives predate complete table coverage. Restore clears these
    /// tables too, rather than silently retaining unrelated target contents.
    #[must_use]
    pub fn missing_tables(&self) -> Vec<&'static str> {
        DUMP_TABLES
            .iter()
            .copied()
            .filter(|name| !OPTIONAL_LEGACY_TABLES.contains(name))
            .filter(|name| !self.tables.iter().any(|t| t.name == *name))
            .collect()
    }
}

/// Maximum compressed AND decoded bytes. Files are streamed rather than
/// loading the compressed input alongside every retained row.
pub const MAX_DUMP_BYTES: u64 = 1024 * 1024 * 1024;
/// Check before growing the line buffer, including on highly compressible input.
pub const MAX_DUMP_LINE_BYTES: u64 = 8 * 1024 * 1024;
/// Conservative retained-value budget, counting keys and per-value overhead,
/// not merely their serialized bytes. Restore still buffers validated rows.
pub const MAX_DUMP_RETAINED_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct InspectLimits {
    compressed: u64,
    decoded: u64,
    line: u64,
    retained: u64,
}

const INSPECT_LIMITS: InspectLimits = InspectLimits {
    compressed: MAX_DUMP_BYTES,
    decoded: MAX_DUMP_BYTES,
    line: MAX_DUMP_LINE_BYTES,
    retained: MAX_DUMP_RETAINED_BYTES,
};

// Both paths count the newline, not just the JSON payload. Check before
// growing a buffer or handing another line to the compressor.
fn reserve_decoded_bytes(
    decoded: &mut u64,
    line_len: u64,
    added: u64,
    limits: InspectLimits,
) -> Result<(), DumpError> {
    if added > limits.line.saturating_sub(line_len) {
        return Err(refuse(format!(
            "dump decoded line exceeds {}-byte cap",
            limits.line
        )));
    }
    if added > limits.decoded.saturating_sub(*decoded) {
        return Err(refuse(format!(
            "dump decoded content exceeds {}-byte cap",
            limits.decoded
        )));
    }
    *decoded += added;
    Ok(())
}

fn read_capped_line(
    reader: &mut impl BufRead,
    decoded: &mut u64,
    limits: InspectLimits,
) -> Result<Option<Vec<u8>>, DumpError> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let n = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        reserve_decoded_bytes(decoded, line.len() as u64, n as u64, limits)?;
        let complete = bytes[n - 1] == b'\n';
        line.extend_from_slice(&bytes[..n]);
        reader.consume(n);
        if complete {
            return Ok(Some(line));
        }
    }
}

// Deliberately over-count value/map/vector bookkeeping so tiny native JSON
// nodes cannot evade the cumulative cap. Per-line input is bounded separately.
fn retained_weight(value: &Value) -> u64 {
    128 + match value {
        Value::String(s) => s.capacity() as u64,
        Value::Array(items) => items.iter().map(retained_weight).sum(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| k.capacity() as u64 + retained_weight(v))
            .sum(),
        _ => 0,
    }
}

fn retain_within_cap(retained: &mut u64, value: &Value, limit: u64) -> Result<(), DumpError> {
    *retained += retained_weight(value);
    if *retained > limit {
        return Err(refuse(format!(
            "dump retained data exceeds {limit}-byte cap"
        )));
    }
    Ok(())
}

fn ensure_within_size_cap(len: u64, what: &str) -> Result<(), DumpError> {
    if len > MAX_DUMP_BYTES {
        return Err(refuse(format!(
            "{what} is {len} bytes, past the {MAX_DUMP_BYTES}-byte dump cap; \
             refusing rather than buffering it"
        )));
    }
    Ok(())
}

/// A file-level refusal: the dump is not ours, truncated, or internally
/// inconsistent. Never a database error; see [`super::dump`] for those.
#[derive(Debug, Error)]
pub enum DumpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("gzip: {0}")]
    Gzip(String),
    #[error("json on line {line}: {message}")]
    Json { line: u64, message: String },
    #[error("{0}")]
    Refused(String),
}

/// True when `name` is a table this backup format owns.
///
/// The restore interpolates table names into SQL, and a backup file is not a
/// trusted input — bytes off a disk someone else may have written. Without
/// this gate a crafted dump naming `website_users` would be
/// truncated-and-inserted like one of ours.
#[must_use]
pub fn is_dump_table(name: &str) -> bool {
    DUMP_TABLES.contains(&name)
}

/// Tombstone high-water marks compare as TEXT, so restore must preserve the
/// exact fixed-width millisecond UTC form, not merely a parseable instant.
#[must_use]
pub fn is_canonical_tombstone_timestamp(value: Option<&Value>) -> bool {
    let Some(text) = value
        .and_then(Value::as_str)
        .filter(|text| text.len() == 24)
    else {
        return false;
    };
    let Ok(instant) =
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
    else {
        return false;
    };
    crate::funnel::format_iso_millis((instant.unix_timestamp_nanos() / 1_000_000) as i64) == text
}

fn refuse(message: impl Into<String>) -> DumpError {
    DumpError::Refused(message.into())
}

/// Write one dump line (already-serialised JSON + newline) into the encoder.
pub fn write_line(enc: &mut GzEncoder<Vec<u8>>, value: &Value) -> Result<(), DumpError> {
    let mut line = serde_json::to_vec(value)
        .map_err(|e| refuse(format!("cannot serialise dump line: {e}")))?;
    line.push(b'\n');
    enc.write_all(&line)?;
    Ok(())
}

/// Finish a gzip stream and return the compressed bytes.
pub fn finish_gzip(enc: GzEncoder<Vec<u8>>) -> Result<Vec<u8>, DumpError> {
    enc.finish().map_err(|e| DumpError::Gzip(e.to_string()))
}

// Unlike the fixture helpers above, the live writer is bounded while encoding
// and never writes under a backup filename until the exact reader accepts it.
#[cfg(any(feature = "db", test))]
struct CappedWrite<W> {
    inner: W,
    written: u64,
    limit: u64,
    what: &'static str,
}

#[cfg(any(feature = "db", test))]
impl<W: Write> Write for CappedWrite<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.written) {
            return Err(std::io::Error::other(format!(
                "dump {} exceeds {}-byte cap",
                self.what, self.limit
            )));
        }
        let n = self.inner.write(bytes)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(any(feature = "db", test))]
struct TemporaryDump(std::path::PathBuf);

#[cfg(any(feature = "db", test))]
impl Drop for TemporaryDump {
    fn drop(&mut self) {
        // Error/cancellation unwinding removes only our exclusively-created
        // temporary. A killed process can leave a .tmp, never a backup candidate.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Bounded, unpublished live archive. Dropping it never publishes a prefix.
#[cfg(any(feature = "db", test))]
pub(crate) struct DumpWriter {
    enc: GzEncoder<CappedWrite<std::fs::File>>,
    temporary: TemporaryDump,
    destination: std::path::PathBuf,
    decoded: u64,
    retained: u64,
    limits: InspectLimits,
}

#[cfg(any(feature = "db", test))]
impl DumpWriter {
    pub(crate) fn new(destination: &Path) -> Result<Self, DumpError> {
        Self::with_limits(destination, INSPECT_LIMITS)
    }

    fn with_limits(destination: &Path, limits: InspectLimits) -> Result<Self, DumpError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        loop {
            // Neither retention's two-funnel-*.ndjson.gz predicate nor the
            // restore drill's glob can match this name. Same directory = same FS.
            let path = parent.join(format!(
                ".dump-writing-{}-{stamp}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // Exclusive creation also refuses symlinks (including dangling ones).
            // https://doc.rust-lang.org/std/fs/struct.OpenOptions.html#method.create_new
            let file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            };
            return Ok(Self {
                enc: GzEncoder::new(
                    CappedWrite {
                        inner: file,
                        written: 0,
                        limit: limits.compressed,
                        what: "compressed content",
                    },
                    Compression::new(9),
                ),
                temporary: TemporaryDump(path),
                destination: destination.to_owned(),
                decoded: 0,
                retained: 0,
                limits,
            });
        }
    }

    pub(crate) fn write_line(&mut self, value: &Value) -> Result<(), DumpError> {
        // Bound JSON escaping before allocation (a source cell can expand on
        // serialization). Reserve room for the newline in the same line cap.
        let mut line = CappedWrite {
            inner: Vec::new(),
            written: 0,
            limit: self.limits.line,
            what: "decoded line",
        };
        serde_json::to_writer(&mut line, value)
            .map_err(|e| refuse(format!("cannot serialise dump line: {e}")))?;
        line.write_all(b"\n")?;
        reserve_decoded_bytes(&mut self.decoded, 0, line.written, self.limits)?;
        // The reader budgets allocated capacities, not just string lengths.
        // Parse the actual bytes so writer/reader accounting sees the same
        // capacities, rather than capacities inherited from database values.
        let parsed: Value = serde_json::from_slice(&line.inner)
            .map_err(|e| refuse(format!("cannot parse encoded dump line: {e}")))?;
        if matches!(
            parsed.get("kind").and_then(Value::as_str),
            Some("manifest" | "row")
        ) {
            retain_within_cap(&mut self.retained, &parsed, self.limits.retained)?;
        }
        self.enc.write_all(&line.inner)?;
        Ok(())
    }

    pub(crate) fn publish(self) -> Result<(), DumpError> {
        let Self {
            enc,
            temporary,
            destination,
            limits,
            ..
        } = self;
        // finish includes the gzip trailer and propagates short/failed writes.
        // https://docs.rs/flate2/1.1.10/flate2/write/struct.GzEncoder.html#method.finish
        let mut output = enc.finish()?;
        output.flush()?;
        // Drop alone ignores close-time errors; sync before validation/rename.
        // https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all
        output.inner.sync_all()?;
        drop(output);
        // The full structural/count/gzip validation is exactly restore's reader,
        // not merely the writer's expectation of what it emitted.
        drop(inspect_with_limits(&temporary.0, limits)?);
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let directory = std::fs::File::open(parent)?;
        rename_no_replace(&temporary.0, &destination)?;
        // File fsync does not persist the renamed directory entry. Require
        // that durability too before CLI retention can remove recovery points.
        // https://man7.org/linux/man-pages/man2/fsync.2.html
        directory.sync_all()?;
        Ok(())
    }
}

// std::fs::rename replaces existing destinations on Unix. Use Linux's atomic
// no-replace rename instead: even racing publishers cannot destroy a good dump.
// https://man7.org/linux/man-pages/man2/rename.2.html (RENAME_NOREPLACE)
#[cfg(all(any(feature = "db", test), target_os = "linux"))]
#[allow(unsafe_code)]
fn rename_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn renameat2(
            oldfd: i32,
            old: *const std::ffi::c_char,
            newfd: i32,
            new: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    let path = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    };
    let source = path(source)?;
    let destination = path(destination)?;
    // Linux UAPI: AT_FDCWD=-100; RENAME_NOREPLACE=1.
    // SAFETY: the `renameat2` declaration matches the Linux UAPI
    // (`int renameat2(int, const char *, int, const char *, unsigned int)`).
    // Both pointers come from live `CString`s that outlive the call, so they
    // are valid NUL-terminated strings; the kernel only reads path bytes and
    // never takes ownership of Rust memory. `last_os_error` is read before any
    // other syscall on the failure path.
    let result = unsafe { renameat2(-100, source.as_ptr(), -100, destination.as_ptr(), 1) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(any(feature = "db", test), not(target_os = "linux")))]
fn rename_no_replace(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    // Fail closed, rather than fall back to an overwrite-prone check + rename.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace dump publication requires Linux renameat2",
    ))
}

/// A row line, with the table name already gated.
#[derive(Debug, Clone)]
pub struct DumpRow {
    pub table: String,
    pub data: Map<String, Value>,
}

/// A fully validated dump: manifest plus rows keyed by table, with the total
/// already checked against the file's own `end` marker.
#[derive(Debug)]
pub struct DumpContents {
    pub manifest: DumpManifest,
    /// Rows read off the file, keyed by table, in file order.
    pub buffers: BTreeMap<String, Vec<Map<String, Value>>>,
    /// Total rows read, already checked against the file's own `end` marker.
    pub rows: u64,
}

/// Read a dump and check that it is internally consistent, touching no database.
///
/// Everything knowable from the file alone is decided here: the format
/// version, that the table names are ours, that the end marker is present,
/// and that the row counts match what the writer declared. `restore()` calls
/// this first so a bad file is refused before a transaction opens — and
/// `restore --dry-run` calls it *instead*, which is what makes the dry run a
/// real check of the backup rather than a check that a URL parses.
pub fn inspect_bytes(bytes: &[u8]) -> Result<DumpContents, DumpError> {
    ensure_within_size_cap(bytes.len() as u64, "dump")?;
    inspect_reader(bytes, INSPECT_LIMITS)
}

fn inspect_reader(input: impl Read, limits: InspectLimits) -> Result<DumpContents, DumpError> {
    let decoder = GzDecoder::new(input);
    let mut reader = std::io::BufReader::new(decoder);
    let mut manifest: Option<DumpManifest> = None;
    // Per-table manifest-column membership sets, built once when the manifest
    // validates so the row arm below stays O(row keys) per row.
    let mut column_sets: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    let mut saw_end = false;
    let mut declared_rows: u64 = 0;
    let mut buffers: BTreeMap<String, Vec<Map<String, Value>>> = BTreeMap::new();
    let mut decoded = 0;
    let mut retained = 0;
    let mut line_no = 0;

    while let Some(line) = read_capped_line(&mut reader, &mut decoded, limits)? {
        line_no += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut obj: Value = serde_json::from_slice(&line).map_err(|e| DumpError::Json {
            line: line_no,
            message: e.to_string(),
        })?;
        if saw_end {
            return Err(refuse("dump contains data after its end marker"));
        }
        if !obj.is_object() {
            return Err(refuse(format!("dump line {line_no} is not an object")));
        }
        let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("");
        match kind {
            "manifest" => {
                if manifest.is_some() {
                    return Err(refuse("dump contains more than one manifest"));
                }
                let version = obj
                    .get("version")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| refuse("manifest has an invalid version"))?;
                if version != 3
                    && version != 4
                    && version != 5
                    && version != 6
                    && version != 7
                    && version != 8
                    && version != 9
                    && version != 10
                    && version != 11
                    && version != 12
                    && version != 13
                    && version != 14
                    && version != u64::from(DUMP_VERSION)
                {
                    return Err(refuse(format!(
                        "dump version {version}, this build reads 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14 and {DUMP_VERSION}"
                    )));
                }
                retain_within_cap(&mut retained, &obj, limits.retained)?;
                let validated = validate_manifest(&obj)?;
                // Build the membership sets once (O(total columns)): testing
                // each row key against them is O(row keys), while a linear
                // `columns.contains` per key would be O(keys x columns) per
                // row and let a hostile archive hang inspect inside the byte
                // caps of docs/backup.md.
                let mut sets = BTreeMap::new();
                for table in &validated.tables {
                    sets.insert(
                        table.name.clone(),
                        table.columns.iter().cloned().collect::<HashSet<String>>(),
                    );
                }
                column_sets = sets;
                manifest = Some(validated);
            }
            "row" => {
                let Some(m) = manifest.as_ref() else {
                    return Err(refuse("dump row appears before the manifest"));
                };
                let table = obj
                    .get("table")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if !is_dump_table(&table) {
                    return Err(refuse(format!(
                        "row: {table:?} is not a table this backup format owns \
                         (expected one of {})",
                        DUMP_TABLES.join(", ")
                    )));
                }
                let info = m.tables.iter().find(|t| t.name == table).ok_or_else(|| {
                    refuse(format!("row table {table} is not declared in the manifest"))
                })?;
                retain_within_cap(&mut retained, &obj, limits.retained)?;
                let data = match obj.get_mut("data").map(Value::take) {
                    Some(Value::Object(data)) => data,
                    _ => return Err(refuse("dump row has no data object")),
                };
                // The manifest column list gates row keys here, before any
                // publish/restore or `--dry-run` success: a row key it never
                // declared refuses, and so does a row missing a column it
                // did declare. Both messages are fixed vocabulary: they
                // never echo row content.
                // Membership is tested against the per-table set built with the
                // manifest, so the per-row cost is O(row keys). The fallback
                // linear scan is unreachable (the set is built from the same
                // manifest `info` came from) and only preserves the refusal if
                // that invariant ever breaks.
                let has_outside_key = match column_sets.get(&table) {
                    Some(known) => data.keys().any(|key| !known.contains(key)),
                    None => data.keys().any(|key| !info.columns.contains(key)),
                };
                if has_outside_key {
                    return Err(refuse(format!(
                        "{table} row on line {line_no} has a column outside the manifest"
                    )));
                }
                // Completeness is the other half of the restore contract:
                // restore binds one parameter per kept manifest column and
                // refuses a row with a missing cell (`missing cell`,
                // dump.rs), so a row carrying a subset of its table's
                // declared columns refuses here too. The check runs for
                // every dump version including frozen v3, whose untyped
                // rows still declare columns; the message names neither
                // the missing column nor any cell value.
                // Outside keys have already refused, so equality with the
                // deduplicated set's size proves completeness in O(1), even
                // when the manifest repeats a column many times.
                let is_missing_column = match column_sets.get(&table) {
                    Some(known) => data.len() != known.len(),
                    None => info.columns.iter().any(|col| !data.contains_key(col)),
                };
                if is_missing_column {
                    return Err(refuse(format!(
                        "{table} row on line {line_no} is missing a manifest column"
                    )));
                }
                if table == "scheduled_events_tombstone"
                    && !is_canonical_tombstone_timestamp(data.get("deleted_at"))
                {
                    return Err(refuse(
                        "scheduled_events_tombstone row requires canonical millisecond UTC deleted_at",
                    ));
                }
                // Frozen v3 has native driver-decoded cells and no type metadata.
                // This port's text-output encoding must remain strings/nulls.
                if !info.column_types.is_empty() {
                    for (col, value) in &data {
                        if !(value.is_string() || value.is_null()) {
                            return Err(refuse(format!(
                                "{table} row on line {line_no}: column {col:?} is not a string or null \
                                 in a text-encoded dump"
                            )));
                        }
                    }
                }
                let rows = buffers.entry(table).or_default();
                if rows.len() as u64 >= info.count {
                    return Err(refuse(format!(
                        "{}: rows exceed manifest count {}",
                        info.name, info.count
                    )));
                }
                rows.push(data);
            }
            "end" => {
                // as_u64 refuses nulls, strings, negatives and floating-point
                // values. Missing metadata must not verify an empty archive.
                // https://docs.rs/serde_json/1.0.151/serde_json/value/enum.Value.html#method.as_u64
                declared_rows = obj
                    .get("rows")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| refuse("dump end marker has an invalid row count"))?;
                saw_end = true;
            }
            _ => {
                return Err(refuse(format!(
                    "dump line {line_no} has unknown kind {kind:?}"
                )));
            }
        }
    }

    let Some(m) = manifest else {
        return Err(refuse(
            "no manifest: not a two-bot dump, or the file is truncated",
        ));
    };
    // A dump that stops mid-file is the disk-full case. Refuse it rather than
    // restoring a prefix of the data and calling it a success.
    if !saw_end {
        return Err(refuse(
            "dump has no end marker - it is truncated, treat it as lost",
        ));
    }

    let mut read_rows: u64 = 0;
    for table in &m.tables {
        let actual = buffers.get(&table.name).map(Vec::len).unwrap_or(0) as u64;
        if actual != table.count {
            return Err(refuse(format!(
                "{}: manifest declares {} rows, file contains {actual}",
                table.name, table.count
            )));
        }
        read_rows += actual;
    }
    if read_rows != declared_rows {
        return Err(refuse(format!(
            "dump declares {declared_rows} rows, file contains {read_rows}"
        )));
    }

    Ok(DumpContents {
        manifest: m,
        buffers,
        rows: read_rows,
    })
}

/// Read and validate a dump file from disk.
pub fn inspect(path: &Path) -> Result<DumpContents, DumpError> {
    inspect_with_limits(path, INSPECT_LIMITS)
}

fn inspect_with_limits(path: &Path, limits: InspectLimits) -> Result<DumpContents, DumpError> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > limits.compressed {
        return Err(refuse(format!(
            "dump file {} is {len} bytes, past the {}-byte dump cap; refusing rather than buffering it",
            path.display(), limits.compressed
        )));
    }
    // Also bound reads if a file grows after the metadata check.
    inspect_reader(file.take(limits.compressed), limits)
}

fn validate_manifest(obj: &Value) -> Result<DumpManifest, DumpError> {
    if obj.get("createdAt").and_then(Value::as_str).is_none() {
        return Err(refuse("manifest has no createdAt timestamp"));
    }
    if obj
        .get("eventsSequence")
        .and_then(Value::as_i64)
        .is_none_or(|mark| mark < 0)
    {
        return Err(refuse("manifest has an invalid eventsSequence"));
    }
    if !obj
        .get("schemaMigrations")
        .and_then(Value::as_array)
        .is_some_and(|migrations| migrations.iter().all(Value::is_string))
    {
        return Err(refuse("manifest has an invalid schemaMigrations list"));
    }
    // Frozen v3 omits this additive metadata. When present, do not silently
    // ignore malformed marks just because this reader restores only events.
    if let Some(sequences) = obj.get("sequences") {
        let sequences = sequences
            .as_object()
            .ok_or_else(|| refuse("manifest has invalid sequences"))?;
        for (name, mark) in sequences {
            if !is_dump_table(name) {
                return Err(refuse(format!(
                    "manifest sequence {name:?} is not a table this backup format owns"
                )));
            }
            if mark.as_i64().is_none_or(|mark| mark < 0) {
                return Err(refuse(format!(
                    "manifest sequence {name} has an invalid high-water mark"
                )));
            }
        }
    }
    if let Some(marks) = obj.get("sequenceMarks") {
        validate_sequence_marks(marks)?;
    }
    let tables = obj
        .get("tables")
        .and_then(Value::as_array)
        .ok_or_else(|| refuse("manifest has no table list"))?;
    let mut names = std::collections::BTreeSet::new();
    for table in tables {
        let row = table
            .as_object()
            .ok_or_else(|| refuse("manifest table is not an object"))?;
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| refuse("manifest table has no name"))?;
        if !is_dump_table(name) {
            return Err(refuse(format!(
                "manifest table: {name:?} is not a table this backup format owns \
                 (expected one of {})",
                DUMP_TABLES.join(", ")
            )));
        }
        if !names.insert(name.to_owned()) {
            return Err(refuse(format!("manifest table {name} is duplicated")));
        }
        let columns = row
            .get("columns")
            .and_then(Value::as_array)
            .ok_or_else(|| refuse(format!("manifest table {name} has an invalid column list")))?;
        if columns.iter().any(|c| !c.is_string()) {
            return Err(refuse(format!(
                "manifest table {name} has an invalid column list"
            )));
        }
        // `column_types` is parallel to `columns`: when present and non-empty
        // its length must match, or the restore cast for some column is
        // ambiguous. Absent or empty stays legacy-readable (frozen v3 omits it
        // entirely). Fixed vocabulary: never echo the hostile type entries.
        if let Some(types) = row.get("column_types") {
            let types = types.as_array().ok_or_else(|| {
                refuse(format!(
                    "manifest table {name} has a malformed column type list"
                ))
            })?;
            if types.iter().any(|t| !t.is_string()) {
                return Err(refuse(format!(
                    "manifest table {name} has a malformed column type list"
                )));
            }
            if !types.is_empty() && types.len() != columns.len() {
                return Err(refuse(format!(
                    "manifest table {name} has a column/column_types length mismatch"
                )));
            }
        }
        let count = row
            .get("count")
            .and_then(Value::as_u64)
            .ok_or_else(|| refuse(format!("manifest table {name} has an invalid row count")))?;
        let _ = count;
    }
    // Historical inventories are pinned by fixtures. Each old version waives
    // only tables first covered by a later version. New v15 archives require
    // the full inventory, including both guild counters (0441/0442/0446/0447),
    // the epoch-claim guard (0450/0451), the delete tombstones (0452/0453),
    // the token registry (0456/0457), the event voice pair (0455) and
    // the starboard tables (0458/0459) and giveaways (0460/0461).
    let version = obj.get("version").and_then(Value::as_u64);
    let missing: Vec<&str> = DUMP_TABLES
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, name)| !matches!(version, Some(3..=14)) || !V15_ADDED_TABLES.contains(name))
        .filter(|(_, name)| !matches!(version, Some(3..=13)) || !V14_ADDED_TABLES.contains(name))
        .filter(|(_, name)| !matches!(version, Some(3..=12)) || !V13_ADDED_TABLES.contains(name))
        .filter(|(index, name)| match version {
            Some(3) => *index < 22,
            Some(4) => {
                !V5_ADDED_TABLES.contains(name)
                    && !V6_ADDED_TABLES.contains(name)
                    && !V7_ADDED_TABLES.contains(name)
                    && !V8_ADDED_TABLES.contains(name)
                    && !V9_ADDED_TABLES.contains(name)
                    && !V10_ADDED_TABLES.contains(name)
                    && !V11_ADDED_TABLES.contains(name)
                    && !V12_ADDED_TABLES.contains(name)
                    && !V13_ADDED_TABLES.contains(name)
                    && !OPTIONAL_LEGACY_TABLES.contains(name)
            }
            Some(5) => {
                !V6_ADDED_TABLES.contains(name)
                    && !V7_ADDED_TABLES.contains(name)
                    && !V8_ADDED_TABLES.contains(name)
                    && !V9_ADDED_TABLES.contains(name)
                    && !V10_ADDED_TABLES.contains(name)
                    && !V11_ADDED_TABLES.contains(name)
                    && !V12_ADDED_TABLES.contains(name)
                    && !V13_ADDED_TABLES.contains(name)
                    && !OPTIONAL_LEGACY_TABLES.contains(name)
            }
            Some(6) => {
                !V7_ADDED_TABLES.contains(name)
                    && !V8_ADDED_TABLES.contains(name)
                    && !V9_ADDED_TABLES.contains(name)
                    && !V10_ADDED_TABLES.contains(name)
                    && !V11_ADDED_TABLES.contains(name)
                    && !V12_ADDED_TABLES.contains(name)
                    && !V13_ADDED_TABLES.contains(name)
                    && !OPTIONAL_LEGACY_TABLES.contains(name)
            }
            Some(7) => {
                !V8_ADDED_TABLES.contains(name)
                    && !V9_ADDED_TABLES.contains(name)
                    && !V10_ADDED_TABLES.contains(name)
                    && !V11_ADDED_TABLES.contains(name)
                    && !V12_ADDED_TABLES.contains(name)
                    && !V13_ADDED_TABLES.contains(name)
                    && !OPTIONAL_LEGACY_TABLES.contains(name)
            }
            Some(8) => {
                !V9_ADDED_TABLES.contains(name)
                    && !V10_ADDED_TABLES.contains(name)
                    && !V11_ADDED_TABLES.contains(name)
                    && !V12_ADDED_TABLES.contains(name)
                    && !V13_ADDED_TABLES.contains(name)
                    && !OPTIONAL_LEGACY_TABLES.contains(name)
            }
            Some(9) => {
                !V10_ADDED_TABLES.contains(name)
                    && !V11_ADDED_TABLES.contains(name)
                    && !V12_ADDED_TABLES.contains(name)
                    && !V13_ADDED_TABLES.contains(name)
                    && !OPTIONAL_LEGACY_TABLES.contains(name)
            }
            Some(10) => {
                !V11_ADDED_TABLES.contains(name)
                    && !V12_ADDED_TABLES.contains(name)
                    && !V13_ADDED_TABLES.contains(name)
                    && !OPTIONAL_LEGACY_TABLES.contains(name)
            }
            Some(11) => {
                !V12_ADDED_TABLES.contains(name)
                    && !V13_ADDED_TABLES.contains(name)
                    && !OPTIONAL_LEGACY_TABLES.contains(name)
            }
            Some(12) => !V13_ADDED_TABLES.contains(name) && !OPTIONAL_LEGACY_TABLES.contains(name),
            _ => !OPTIONAL_LEGACY_TABLES.contains(name),
        })
        .map(|(_, name)| name)
        .filter(|name| !names.contains(*name))
        .collect();
    if !missing.is_empty() {
        return Err(refuse(format!(
            "manifest is missing tables: {}",
            missing.join(", ")
        )));
    }
    serde_json::from_value(obj.clone()).map_err(|e| refuse(format!("manifest is malformed: {e}")))
}

/// Restore raises allocators to these marks, so a malformed one must refuse
/// rather than silently restore without the high-water it was meant to carry.
fn validate_sequence_marks(marks: &Value) -> Result<(), DumpError> {
    let marks = marks
        .as_array()
        .ok_or_else(|| refuse("manifest has invalid sequenceMarks"))?;
    let mut keys = std::collections::BTreeSet::new();
    for mark in marks {
        let mark = mark
            .as_object()
            .ok_or_else(|| refuse("manifest sequence mark is not an object"))?;
        let table = mark
            .get("table")
            .and_then(Value::as_str)
            .filter(|table| is_dump_table(table))
            .ok_or_else(|| refuse("manifest sequence mark names no backup table"))?;
        let column = mark
            .get("column")
            .and_then(Value::as_str)
            .filter(|column| !column.is_empty())
            .ok_or_else(|| refuse(format!("manifest sequence mark on {table} has no column")))?;
        if mark.get("lastValue").and_then(Value::as_i64).is_none()
            || mark.get("isCalled").and_then(Value::as_bool).is_none()
            || mark
                .get("increment")
                .and_then(Value::as_i64)
                .is_none_or(|increment| increment == 0)
        {
            return Err(refuse(format!(
                "manifest sequence mark {table}.{column} has an invalid position"
            )));
        }
        if !keys.insert((table, column)) {
            return Err(refuse(format!(
                "manifest sequence mark {table}.{column} is duplicated"
            )));
        }
    }
    Ok(())
}

/// Convert a validated cell to bound PostgreSQL input, never coercing an
/// unsupported native value into SQL NULL. Legacy's driver used native scalar
/// JSON; this port marks its PostgreSQL text-output encoding with column types.
#[cfg(any(feature = "db", test))]
pub(crate) fn cell_input(
    value: &Value,
    target_type: &str,
    text_encoded: bool,
) -> Result<Option<String>, DumpError> {
    if value.is_null() {
        return Ok(None);
    }
    if !text_encoded && matches!(target_type, "json" | "jsonb") {
        return serde_json::to_string(value)
            .map(Some)
            .map_err(|e| refuse(format!("cannot encode native JSON cell: {e}")));
    }
    match value {
        Value::String(s) => Ok(Some(s.clone())),
        Value::Number(_) | Value::Bool(_) if !text_encoded => Ok(Some(value.to_string())),
        _ => Err(refuse(format!(
            "cannot encode native cell for PostgreSQL {target_type}; refusing rather than restoring NULL"
        ))),
    }
}

/// Fresh gzip encoder at the legacy compression level (9).
#[must_use]
pub fn new_encoder() -> GzEncoder<Vec<u8>> {
    GzEncoder::new(Vec::new(), Compression::new(9))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder as WEnc;

    // Frozen released v12 inventory: registry present, no starboard tables.
    const FROZEN_V12_TABLES: [&str; 99] = [
        "events",
        "members",
        "invite_snapshots",
        "operational_audit_log",
        "moderation_warnings",
        "moderation_scheduled_unbans",
        "moderation_audit",
        "moderation_lockdowns",
        "moderation_idempotency",
        "containment_events",
        "containment_incidents",
        "join_risk_flags",
        "automation_commands",
        "scheduled_messages",
        "sticky_messages",
        "automation_audit_log",
        "tickets",
        "ticket_transcripts",
        "automod_violations",
        "automod_processed_messages",
        "self_role_audit",
        "self_role_panel_claims",
        "self_role_exchanges",
        "self_role_exchange_baselines",
        "automod_delivery_claims",
        "gateway_boot_directives",
        "member_erasure_audit",
        "privacy_opt_outs",
        "guild_analytics_settings",
        "privacy_opt_out_audit",
        "game_activity_guild_aggregates",
        "assistant_monthly_builds",
        "member_levels",
        "member_season_xp",
        "xp_awards",
        "level_role_rewards",
        "level_import_runs",
        "event_rsvps",
        "announcements_audit_log",
        "community_facts",
        "lfg_posts",
        "lfg_roles",
        "lfg_signups",
        "feed_relays",
        "feed_deliveries",
        "web_contract_meta",
        "guild_counters",
        "rank_ladder",
        "rank_snapshots",
        "member_ranks",
        "scheduled_events",
        "scheduled_events_claim",
        "scheduled_events_tombstone",
        "counter_snapshots",
        "member_exclusions",
        "presence_probe",
        "community_stream_heartbeats",
        "community_scorecard_runs",
        "community_scorecard_alerts",
        "community_scorecard_attempts",
        "gateway_sessions",
        "guild_settings_revision",
        "guild_settings",
        "guild_settings_audit",
        "admin_api_tokens",
        "audit_kill_switch",
        "internal_nonces",
        "internal_clock_high_water",
        "internal_idempotency",
        "internal_action_log",
        "internal_discord_events",
        "internal_event_keys",
        "stream_live_claims",
        "moderation_channel_executions",
        "moderation_member_bans",
        "invite_campaigns",
        "voice_creators",
        "voice_rooms",
        "voice_room_blocks",
        "voice_join_grants",
        "voice_owner_grants",
        "voice_text_companions",
        "voice_access_controls",
        "voice_logging_settings",
        "voice_channel_templates",
        "voice_game_aliases",
        "voice_random_lists",
        "voice_random_list_choices",
        "voice_logging",
        "voice_logging_mention_members",
        "voice_logging_mention_roles",
        "voice_guild_settings",
        "voice_command_roles",
        "voice_command_role_members",
        "voice_vote_kick_audit",
        "voice_create_reservations",
        "retention_guild_purge_queue",
        "retention_deletion_requests",
        "event_store",
    ];

    // Frozen released v13: the voice pair and token registry, before starboard.
    const FROZEN_V13_TABLES: [&str; 101] = [
        "events",
        "members",
        "invite_snapshots",
        "operational_audit_log",
        "moderation_warnings",
        "moderation_scheduled_unbans",
        "moderation_audit",
        "moderation_lockdowns",
        "moderation_idempotency",
        "containment_events",
        "containment_incidents",
        "join_risk_flags",
        "automation_commands",
        "scheduled_messages",
        "sticky_messages",
        "automation_audit_log",
        "tickets",
        "ticket_transcripts",
        "automod_violations",
        "automod_processed_messages",
        "self_role_audit",
        "self_role_panel_claims",
        "self_role_exchanges",
        "self_role_exchange_baselines",
        "automod_delivery_claims",
        "gateway_boot_directives",
        "member_erasure_audit",
        "privacy_opt_outs",
        "guild_analytics_settings",
        "privacy_opt_out_audit",
        "game_activity_guild_aggregates",
        "assistant_monthly_builds",
        "member_levels",
        "member_season_xp",
        "xp_awards",
        "level_role_rewards",
        "level_import_runs",
        "event_rsvps",
        "announcements_audit_log",
        "community_facts",
        "lfg_posts",
        "lfg_roles",
        "lfg_signups",
        "feed_relays",
        "feed_deliveries",
        "web_contract_meta",
        "guild_counters",
        "rank_ladder",
        "rank_snapshots",
        "member_ranks",
        "scheduled_events",
        "scheduled_events_claim",
        "scheduled_events_tombstone",
        "event_voice_channels",
        "event_voice_presence",
        "counter_snapshots",
        "member_exclusions",
        "presence_probe",
        "community_stream_heartbeats",
        "community_scorecard_runs",
        "community_scorecard_alerts",
        "community_scorecard_attempts",
        "gateway_sessions",
        "guild_settings_revision",
        "guild_settings",
        "guild_settings_audit",
        "admin_api_tokens",
        "audit_kill_switch",
        "internal_nonces",
        "internal_clock_high_water",
        "internal_idempotency",
        "internal_action_log",
        "internal_discord_events",
        "internal_event_keys",
        "stream_live_claims",
        "moderation_channel_executions",
        "moderation_member_bans",
        "invite_campaigns",
        "voice_creators",
        "voice_rooms",
        "voice_room_blocks",
        "voice_join_grants",
        "voice_owner_grants",
        "voice_text_companions",
        "voice_access_controls",
        "voice_logging_settings",
        "voice_channel_templates",
        "voice_game_aliases",
        "voice_random_lists",
        "voice_random_list_choices",
        "voice_logging",
        "voice_logging_mention_members",
        "voice_logging_mention_roles",
        "voice_guild_settings",
        "voice_command_roles",
        "voice_command_role_members",
        "voice_vote_kick_audit",
        "voice_create_reservations",
        "retention_guild_purge_queue",
        "retention_deletion_requests",
        "event_store",
    ];

    fn complete_tables(overrides: &BTreeMap<&str, (Vec<&str>, u64)>) -> Vec<Value> {
        DUMP_TABLES
            .iter()
            .map(|name| {
                let (cols, count) = overrides.get(name).cloned().unwrap_or_default();
                serde_json::json!({
                    "name": name,
                    "columns": cols,
                    "column_types": cols.iter().map(|_| "text").collect::<Vec<_>>(),
                    "count": count,
                })
            })
            .collect()
    }

    fn manifest(tables: Vec<Value>) -> Value {
        serde_json::json!({
            "kind": "manifest",
            "version": DUMP_VERSION,
            "createdAt": "2026-08-25T00:00:00.000Z",
            "tables": tables,
            "eventsSequence": 1,
            "schemaMigrations": ["001"],
        })
    }

    fn gzip_lines(objs: &[Value]) -> Vec<u8> {
        let mut enc = WEnc::new(Vec::new(), Compression::new(9));
        for obj in objs {
            let mut line = serde_json::to_vec(obj).unwrap();
            line.push(b'\n');
            enc.write_all(&line).unwrap();
        }
        enc.finish().unwrap()
    }

    fn good_dump() -> Vec<Value> {
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id", "guild_id"], 1));
        vec![
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"events","data":{"id":"1","guild_id":"g"}}),
            serde_json::json!({"kind":"end","rows":1}),
        ]
    }

    fn empty_dump() -> Vec<Value> {
        vec![
            manifest(complete_tables(&BTreeMap::new())),
            serde_json::json!({"kind":"end","rows":0}),
        ]
    }

    #[test]
    fn accepts_empty_archives_with_zero_sequence_and_empty_migrations() {
        let mut objs = empty_dump();
        objs[0]["eventsSequence"] = serde_json::json!(0);
        objs[0]["schemaMigrations"] = serde_json::json!([]);
        // Legacy v3 does not emit column_types or sequences.
        for table in objs[0]["tables"].as_array_mut().unwrap() {
            table.as_object_mut().unwrap().shift_remove("column_types");
        }
        let contents = inspect_bytes(&gzip_lines(&objs)).unwrap();
        assert_eq!(contents.rows, 0);
        assert!(contents.buffers.is_empty());
        assert_eq!(contents.manifest.events_sequence, 0);
        assert!(contents.manifest.schema_migrations.is_empty());
        // Rust retains BIGINT marks exactly, without JavaScript's safe-integer
        // limitation. Present additive metadata must still be well-formed.
        objs[0]["eventsSequence"] = serde_json::json!(i64::MAX);
        objs[0]["sequences"] = serde_json::json!({"events": i64::MAX});
        assert_eq!(
            inspect_bytes(&gzip_lines(&objs))
                .unwrap()
                .manifest
                .events_sequence,
            i64::MAX
        );
    }

    #[test]
    fn refuses_malformed_end_counts_even_in_empty_archives() {
        let mut missing = empty_dump();
        missing[1].as_object_mut().unwrap().shift_remove("rows");
        let err = inspect_bytes(&gzip_lines(&missing)).unwrap_err();
        assert!(err.to_string().contains("invalid row count"), "{err}");
        for rows in [
            Value::Null,
            serde_json::json!("0"),
            serde_json::json!(false),
            serde_json::json!(-1),
            serde_json::json!(0.0),
            serde_json::json!(0.5),
            serde_json::json!(1e20),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let mut objs = empty_dump();
            objs[1]["rows"] = rows.clone();
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(
                err.to_string().contains("invalid row count"),
                "rows={rows}: {err}"
            );
        }
        let mut mismatch = empty_dump();
        mismatch[1]["rows"] = serde_json::json!(u64::MAX);
        let err = inspect_bytes(&gzip_lines(&mismatch)).unwrap_err();
        assert!(err.to_string().contains("file contains 0"), "{err}");
    }

    #[test]
    fn refuses_non_object_records_and_malformed_kinds() {
        for record in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!([{"kind":"end", "rows":0}]),
            serde_json::json!(42),
            serde_json::json!("end"),
            serde_json::json!(true),
        ] {
            for index in [0, 1] {
                let mut objs = empty_dump();
                objs[index] = record.clone();
                let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
                assert!(err.to_string().contains("not an object"), "{err}");
            }
        }
        for record in [
            serde_json::json!({"rows":0}),
            serde_json::json!({"kind":null,"rows":0}),
            serde_json::json!({"kind":[],"rows":0}),
            serde_json::json!({"kind":"checkpoint","rows":0}),
        ] {
            let mut objs = empty_dump();
            objs[1] = record;
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(err.to_string().contains("unknown kind"), "{err}");
        }
    }

    #[test]
    fn refuses_missing_or_mistyped_required_manifest_metadata() {
        for field in [
            "version",
            "createdAt",
            "eventsSequence",
            "schemaMigrations",
            "tables",
        ] {
            let mut objs = empty_dump();
            objs[0].as_object_mut().unwrap().shift_remove(field);
            assert!(
                inspect_bytes(&gzip_lines(&objs)).is_err(),
                "missing {field}"
            );
            for value in [Value::Null, serde_json::json!(true), serde_json::json!({})] {
                let mut objs = empty_dump();
                objs[0][field] = value.clone();
                assert!(
                    inspect_bytes(&gzip_lines(&objs)).is_err(),
                    "{field}={value}"
                );
            }
        }
        for (field, values) in [
            (
                "version",
                vec![
                    serde_json::json!("3"),
                    serde_json::json!(3.0),
                    serde_json::json!(-1),
                ],
            ),
            (
                "createdAt",
                vec![serde_json::json!(42), serde_json::json!([])],
            ),
            (
                "eventsSequence",
                vec![
                    serde_json::json!("0"),
                    serde_json::json!(-1),
                    serde_json::json!(0.0),
                    serde_json::json!(0.5),
                    serde_json::json!(i64::MAX as u64 + 1),
                ],
            ),
            (
                "schemaMigrations",
                vec![
                    serde_json::json!("001"),
                    serde_json::json!([null]),
                    serde_json::json!(["001", 2]),
                    serde_json::json!([[]]),
                ],
            ),
        ] {
            for value in values {
                let mut objs = empty_dump();
                objs[0][field] = value.clone();
                let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
                assert!(err.to_string().contains(field), "{field}={value}: {err}");
            }
        }
    }

    #[test]
    fn refuses_malformed_present_sequence_metadata() {
        for sequences in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!("events"),
            serde_json::json!(0),
            serde_json::json!(true),
            serde_json::json!({"website_users": 1}),
            serde_json::json!({"events": null}),
            serde_json::json!({"events": "0"}),
            serde_json::json!({"events": -1}),
            serde_json::json!({"events": 0.0}),
            serde_json::json!({"events": 0.5}),
            serde_json::json!({"events": i64::MAX as u64 + 1}),
            serde_json::json!({"events": []}),
            serde_json::json!({"events": {}}),
        ] {
            let mut objs = empty_dump();
            objs[0]["sequences"] = sequences.clone();
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(err.to_string().contains("sequence"), "{sequences}: {err}");
        }
    }

    #[test]
    fn sequence_marks_parse_and_report_the_next_allocation() {
        let mut objs = empty_dump();
        objs[0]["sequenceMarks"] = serde_json::json!([
            {"table": "events", "column": "id", "lastValue": 41, "isCalled": true, "increment": 1},
            {"table": "members", "column": "backup_identity", "lastValue": 17, "isCalled": false, "increment": 3},
            {"table": "guild_settings", "column": "cas_version", "lastValue": -9, "isCalled": true, "increment": -1},
            {"table": "xp_awards", "column": "id", "lastValue": i64::MAX, "isCalled": true, "increment": 1},
        ]);
        let marks = inspect_bytes(&gzip_lines(&objs))
            .unwrap()
            .manifest
            .sequence_marks;
        let next: Vec<_> = marks.iter().map(SequenceMark::next).collect();
        assert_eq!(next, vec![Some(42), Some(17), Some(-10), None]);
        // Absent marks (v3, older v4) default to none and are not re-emitted.
        let manifest = inspect_bytes(&gzip_lines(&empty_dump())).unwrap().manifest;
        assert!(manifest.sequence_marks.is_empty());
        assert!(serde_json::to_value(&manifest)
            .unwrap()
            .get("sequenceMarks")
            .is_none());
    }

    #[test]
    fn refuses_malformed_sequence_marks() {
        let good = serde_json::json!({"table": "events", "column": "id", "lastValue": 1, "isCalled": true, "increment": 1});
        let with = |field: &str, value: Value| {
            let mut mark = good.clone();
            mark[field] = value;
            serde_json::json!([mark])
        };
        let mut cases = vec![
            Value::Null,
            serde_json::json!({}),
            serde_json::json!("events"),
            serde_json::json!([1]),
            serde_json::json!([good.clone(), good.clone()]),
            with("table", serde_json::json!("website_users")),
            with("table", Value::Null),
            with("column", serde_json::json!("")),
            with("column", serde_json::json!(1)),
            with("lastValue", serde_json::json!("1")),
            with("lastValue", serde_json::json!(0.5)),
            with("lastValue", serde_json::json!(i64::MAX as u64 + 1)),
            with("isCalled", serde_json::json!("true")),
            with("increment", serde_json::json!(0)),
            with("increment", Value::Null),
        ];
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().shift_remove("isCalled");
        cases.push(serde_json::json!([missing]));
        for marks in cases {
            let mut objs = empty_dump();
            objs[0]["sequenceMarks"] = marks.clone();
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(err.to_string().contains("sequence"), "{marks}: {err}");
        }
    }

    #[test]
    fn refuses_malformed_table_counts_and_typed_column_metadata() {
        for count in [
            Value::Null,
            serde_json::json!("0"),
            serde_json::json!(-1),
            serde_json::json!(0.0),
            serde_json::json!(1e20),
            serde_json::json!([]),
        ] {
            let mut objs = empty_dump();
            objs[0]["tables"][0]["count"] = count;
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(err.to_string().contains("invalid row count"), "{err}");
        }
        for field in ["name", "columns", "count"] {
            let mut objs = empty_dump();
            objs[0]["tables"][0]
                .as_object_mut()
                .unwrap()
                .shift_remove(field);
            assert!(
                inspect_bytes(&gzip_lines(&objs)).is_err(),
                "missing table {field}"
            );
        }
        for types in [
            Value::Null,
            serde_json::json!("text"),
            serde_json::json!([null]),
            serde_json::json!([1]),
        ] {
            let mut objs = empty_dump();
            objs[0]["tables"][0]["column_types"] = types;
            let err = inspect_bytes(&gzip_lines(&objs)).unwrap_err();
            assert!(err.to_string().contains("malformed"), "{err}");
        }
    }

    fn publication_dir(label: &str) -> std::path::PathBuf {
        let scratch = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = scratch.join(format!(
            "dump-publication-{label}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn write_archive(path: &Path, objs: &[Value], limits: InspectLimits) -> Result<(), DumpError> {
        let mut writer = DumpWriter::with_limits(path, limits)?;
        for obj in objs {
            writer.write_line(obj)?;
        }
        writer.publish()
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn live_writer_reader_boundaries_for_every_budget() {
        let mut objs = good_dump();
        objs[1]["data"]["guild_id"] = Value::String("x".repeat(4096));
        objs[0]["tables"][0]["count"] = serde_json::json!(2);
        objs.insert(2, objs[1].clone());
        objs[3]["rows"] = serde_json::json!(2);
        let lines: Vec<Vec<u8>> = objs
            .iter()
            .map(|obj| {
                let mut line = serde_json::to_vec(obj).unwrap();
                line.push(b'\n');
                line
            })
            .collect();
        let exact = InspectLimits {
            compressed: gzip_lines(&objs).len() as u64,
            decoded: lines.iter().map(|l| l.len() as u64).sum(),
            line: lines.iter().map(|l| l.len() as u64).max().unwrap(),
            retained: lines[..lines.len() - 1]
                .iter()
                .map(|l| retained_weight(&serde_json::from_slice(l).unwrap()))
                .sum(),
        };
        let dir = publication_dir("budgets");
        let good = dir.join("two-funnel-good.ndjson.gz");
        write_archive(&good, &objs, exact).expect("at every cap is readable and publishable");
        inspect_with_limits(&good, exact).unwrap();
        inspect(&good).unwrap();
        let saved = std::fs::read(&good).unwrap();
        for (label, limits, message) in [
            (
                "compressed",
                InspectLimits {
                    compressed: exact.compressed - 1,
                    ..INSPECT_LIMITS
                },
                "compressed content",
            ),
            (
                "decoded",
                InspectLimits {
                    decoded: exact.decoded - 1,
                    ..INSPECT_LIMITS
                },
                "decoded content",
            ),
            (
                "line",
                InspectLimits {
                    line: exact.line - 1,
                    ..INSPECT_LIMITS
                },
                "decoded line",
            ),
            (
                "retained",
                InspectLimits {
                    retained: exact.retained - 1,
                    ..INSPECT_LIMITS
                },
                "retained data",
            ),
        ] {
            inspect_with_limits(&good, limits)
                .expect_err("reader refuses the same one-byte budget excess");
            // A budget error must neither create a new backup nor overwrite an
            // existing valid destination. Every temporary is cleaned on refusal.
            let absent = dir.join(format!("two-funnel-{label}.ndjson.gz"));
            for destination in [&absent, &good] {
                let err = write_archive(destination, &objs, limits).unwrap_err();
                assert!(err.to_string().contains(message), "{label}: {err}");
                assert!(!absent.exists());
                assert_eq!(std::fs::read(&good).unwrap(), saved);
                assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn live_writer_validates_counts_before_publication_and_never_clobbers() {
        let dir = publication_dir("validation");
        let path = dir.join("two-funnel-good.ndjson.gz");
        let mut invalid = good_dump();
        invalid[2]["rows"] = serde_json::json!(2);
        let err = write_archive(&path, &invalid, INSPECT_LIMITS).unwrap_err();
        assert!(err.to_string().contains("dump declares 2 rows"), "{err}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        write_archive(&path, &good_dump(), INSPECT_LIMITS).unwrap();
        let saved = std::fs::read(&path).unwrap();
        let err = write_archive(&path, &good_dump(), INSPECT_LIMITS).unwrap_err();
        assert!(
            matches!(err, DumpError::Io(ref e) if e.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        inspect(&path).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn live_writer_refuses_malformed_empty_metadata_before_publication() {
        let dir = publication_dir("empty-metadata");
        let prior = dir.join("two-funnel-prior.ndjson.gz");
        write_archive(&prior, &empty_dump(), INSPECT_LIMITS).unwrap();
        let saved = std::fs::read(&prior).unwrap();
        let next = dir.join("two-funnel-next.ndjson.gz");
        for field in ["end", "manifest"] {
            let mut invalid = empty_dump();
            if field == "end" {
                invalid[1].as_object_mut().unwrap().shift_remove("rows");
            } else {
                invalid[0]["eventsSequence"] = serde_json::json!(-1);
            }
            for path in [&prior, &next] {
                write_archive(path, &invalid, INSPECT_LIMITS).unwrap_err();
                assert!(!next.exists());
                assert_eq!(std::fs::read(&prior).unwrap(), saved);
                assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn abandoned_live_writer_is_not_a_backup_and_cleans_up() {
        let dir = publication_dir("abandoned");
        let path = dir.join("two-funnel-abandoned.ndjson.gz");
        {
            let mut writer = DumpWriter::new(&path).unwrap();
            writer.write_line(&good_dump()[0]).unwrap();
            let temporary = std::fs::read_dir(&dir)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name();
            let name = temporary.to_string_lossy();
            assert!(!name.starts_with("two-funnel-"));
            assert!(!name.ends_with(".ndjson.gz"));
            assert!(!path.exists());
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn capped_output_retries_short_writes_and_propagates_failure() {
        struct ShortWriter(Vec<u8>);
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0.len() == 3 {
                    return Err(std::io::Error::other("injected disk failure"));
                }
                self.0.push(bytes[0]);
                Ok(1)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut out = CappedWrite {
            inner: ShortWriter(Vec::new()),
            written: 0,
            limit: 10,
            what: "test",
        };
        let err = out.write_all(b"abcdef").unwrap_err();
        assert!(err.to_string().contains("injected disk failure"));
        assert_eq!(out.written, 3);
        assert_eq!(out.inner.0, b"abc");
    }

    #[test]
    fn accepts_a_well_formed_dump() {
        let contents = inspect_bytes(&gzip_lines(&good_dump())).expect("good dump");
        assert_eq!(contents.rows, 1);
        assert_eq!(contents.buffers["events"].len(), 1);
        assert_eq!(contents.manifest.version, DUMP_VERSION);
    }

    #[test]
    fn refuses_a_foreign_table_on_the_read_path() {
        // A backup file is not a trusted input; restore() interpolates table
        // names into SQL, so the gate belongs on the read path.
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id"], 0));
        let bytes = gzip_lines(&[
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"website_users","data":{}}),
            serde_json::json!({"kind":"end","rows":0}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("foreign table must be refused");
        assert!(err.to_string().contains("website_users"), "{err}");
    }

    #[test]
    fn refuses_a_truncated_dump_without_end_marker() {
        let objs = good_dump();
        let err = inspect_bytes(&gzip_lines(&objs[..2])).expect_err("truncated");
        assert!(err.to_string().contains("no end marker"), "{err}");
    }

    #[test]
    fn refuses_a_version_mismatch() {
        let mut m = manifest(complete_tables(&BTreeMap::new()));
        m["version"] = serde_json::json!(DUMP_VERSION + 1);
        let bytes = gzip_lines(&[m, serde_json::json!({"kind":"end","rows":0})]);
        let err = inspect_bytes(&bytes).expect_err("version must be refused");
        assert!(err.to_string().contains("dump version"), "{err}");
    }

    #[test]
    fn refuses_data_after_the_end_marker() {
        let mut objs = good_dump();
        objs.push(serde_json::json!({"kind":"row","table":"events","data":{}}));
        let err = inspect_bytes(&gzip_lines(&objs)).expect_err("trailing data");
        assert!(err.to_string().contains("after its end marker"), "{err}");
    }

    #[test]
    fn refuses_a_row_count_mismatch() {
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id"], 2));
        let bytes = gzip_lines(&[
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"events","data":{"id":"1"}}),
            serde_json::json!({"kind":"end","rows":1}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("count mismatch");
        assert!(
            err.to_string().contains("manifest declares 2 rows"),
            "{err}"
        );
    }

    #[test]
    fn refuses_a_manifest_missing_tables() {
        let tables =
            vec![serde_json::json!({"name":"events","columns":[],"column_types":[],"count":0})];
        let bytes = gzip_lines(&[manifest(tables), serde_json::json!({"kind":"end","rows":0})]);
        let err = inspect_bytes(&bytes).expect_err("missing tables");
        assert!(err.to_string().contains("missing tables"), "{err}");
    }

    #[test]
    fn inspects_fixture_emitted_by_the_frozen_legacy_writer() {
        let mut enc = new_encoder();
        enc.write_all(include_bytes!(
            "../../tests/fixtures/legacy-v3-native.ndjson"
        ))
        .unwrap();
        let contents = inspect_bytes(&finish_gzip(enc).unwrap()).unwrap();
        assert_eq!(contents.rows, 2);
        assert_eq!(contents.manifest.version, 3);
        let legacy_names: std::collections::BTreeSet<_> = contents
            .manifest
            .tables
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(legacy_names, DUMP_TABLES.iter().take(22).copied().collect());
        assert_eq!(
            contents.manifest.missing_tables().len(),
            DUMP_TABLES.len() - 22
        );
        assert!(contents
            .manifest
            .tables
            .iter()
            .all(|t| t.column_types.is_empty()));
        assert_eq!(contents.buffers["events"][0]["id"], serde_json::json!(1));
        assert_eq!(
            contents.buffers["join_risk_flags"][0]["score"],
            serde_json::json!(3)
        );
        assert_eq!(
            contents.buffers["join_risk_flags"][0]["flagged"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn inspects_pre_reservation_v4_but_new_archives_require_reservations() {
        let mut enc = new_encoder();
        enc.write_all(include_bytes!(
            "../../tests/fixtures/pre-reservation-v4.ndjson"
        ))
        .unwrap();
        let contents = inspect_bytes(&finish_gzip(enc).unwrap()).unwrap();
        assert_eq!(contents.manifest.version, 4);
        assert_eq!(contents.rows, 1);
        let names: Vec<&str> = contents
            .manifest
            .tables
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        let expected: Vec<&str> = DUMP_TABLES
            .iter()
            .copied()
            .filter(|name| {
                !V5_ADDED_TABLES.contains(name)
                    && !V6_ADDED_TABLES.contains(name)
                    && !V7_ADDED_TABLES.contains(name)
                    && !V8_ADDED_TABLES.contains(name)
                    && !V9_ADDED_TABLES.contains(name)
                    && !V10_ADDED_TABLES.contains(name)
                    && !V11_ADDED_TABLES.contains(name)
                    && !V12_ADDED_TABLES.contains(name)
                    && !V13_ADDED_TABLES.contains(name)
                    && ![
                        "starboard_config",
                        "starboard_starrers",
                        "starboard_posts",
                        "giveaways",
                        "giveaway_entries",
                        "giveaway_draws",
                    ]
                    .contains(name)
            })
            .collect();
        assert_eq!(names, expected);
        // The frozen v4 archive predates every v5 table, in DUMP_TABLES order.
        assert_eq!(
            contents.manifest.missing_tables(),
            [
                "privacy_opt_outs",
                "guild_analytics_settings",
                "privacy_opt_out_audit",
                "game_activity_guild_aggregates",
                "assistant_monthly_builds",
                "member_season_xp",
                "scheduled_events_claim",
                "scheduled_events_tombstone",
                "event_voice_channels",
                "event_voice_presence",
                "admin_api_tokens",
                "stream_live_claims",
                "voice_create_reservations",
                "retention_guild_purge_queue",
                "retention_deletion_requests",
                "event_store",
                "starboard_config",
                "starboard_starrers",
                "starboard_posts",
                "giveaways",
                "giveaway_entries",
                "giveaway_draws"
            ]
        );
        assert_eq!(contents.buffers["voice_rooms"][0]["channel_id"], "500");
        let mut manifest = serde_json::to_value(&contents.manifest).unwrap();
        manifest["version"] = serde_json::json!(DUMP_VERSION);
        let err = validate_manifest(&manifest).unwrap_err().to_string();
        assert!(err.contains("voice_create_reservations"), "{err}");
        assert!(err.contains("retention_guild_purge_queue"), "{err}");
        assert!(err.contains("event_store"), "{err}");
        // V4 has a fixed complete inventory, not a blanket missing-table waiver.
        manifest["version"] = serde_json::json!(4);
        manifest["tables"].as_array_mut().unwrap().remove(0);
        assert!(validate_manifest(&manifest)
            .unwrap_err()
            .to_string()
            .contains("missing tables: events"));
    }

    #[test]
    fn pre_event_store_v5_archives_stay_readable_but_v6_requires_event_store() {
        // A v5 recovery point written before the event-store slice declares
        // every covered table except event_store; v6 made that table mandatory.
        // (The retention queues, the seasonal projection and the stream ledger
        // stay declared here: v5 predates the event-store capture, and the v5
        // waiver covers the v6 event store and the v7/v8 additions.)
        let v5_tables: Vec<Value> = complete_tables(&BTreeMap::new())
            .into_iter()
            .filter(|table| {
                table["name"] != "event_store"
                    && ![
                        "starboard_config",
                        "starboard_starrers",
                        "starboard_posts",
                        "giveaways",
                        "giveaway_entries",
                        "giveaway_draws",
                    ]
                    .iter()
                    .any(|n| table["name"] == *n)
            })
            .collect();
        let mut v5 = manifest(v5_tables);
        v5["version"] = serde_json::json!(5);
        assert!(
            validate_manifest(&v5).is_ok(),
            "v5 predates the event store and stays readable"
        );
        // The inspect read path accepts the frozen v5 envelope too.
        let bytes = gzip_lines(&[v5.clone(), serde_json::json!({"kind":"end","rows":0})]);
        let contents = inspect_bytes(&bytes).expect("v5 without event_store inspects");
        assert_eq!(contents.manifest.version, 5);
        assert_eq!(
            contents.manifest.missing_tables(),
            [
                "event_store",
                "starboard_config",
                "starboard_starrers",
                "starboard_posts",
                "giveaways",
                "giveaway_entries",
                "giveaway_draws"
            ]
        );
        // The same inventory at the current version refuses: new archives must
        // cover the event store.
        assert_eq!(DUMP_VERSION, 15);
        let mut current = v5;
        current["version"] = serde_json::json!(DUMP_VERSION);
        assert!(validate_manifest(&current)
            .unwrap_err()
            .to_string()
            .contains("event_store"));
    }

    #[test]
    fn v5_archives_without_v6_tables_validate_but_v6_requires_them() {
        // A v5 archive carries reservations but predates the v6 event-store
        // capture (0427-0432): it must validate at version 5 and fail at
        // version 6. The v6 set is the event store only; the retention queues
        // (0438) and the seasonal projection are v7 and stay waived at v6
        // (see the frozen v6 pre-retention test below).
        let tables: Vec<Value> = DUMP_TABLES
            .iter()
            .copied()
            .filter(|name| {
                !V6_ADDED_TABLES.contains(name)
                    && ![
                        "starboard_config",
                        "starboard_starrers",
                        "starboard_posts",
                        "giveaways",
                        "giveaway_entries",
                        "giveaway_draws",
                    ]
                    .contains(name)
            })
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "columns": [],
                    "column_types": [],
                    "count": 0,
                })
            })
            .collect();
        let mut v5 = manifest(tables);
        v5["version"] = serde_json::json!(5);
        validate_manifest(&v5).expect("v5 without v6 tables must validate");
        let mut v6 = v5.clone();
        v6["version"] = serde_json::json!(6);
        let err = validate_manifest(&v6).unwrap_err().to_string();
        assert!(err.contains("event_store"), "{err}");
        // A v5 archive missing reservations is still refused: only the v6,
        // v7 and v8 tables are waived at version 5.
        let mut v5_missing_reservation = v5.clone();
        v5_missing_reservation["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|t| t["name"] != "voice_create_reservations");
        assert!(validate_manifest(&v5_missing_reservation)
            .unwrap_err()
            .to_string()
            .contains("voice_create_reservations"));
    }

    #[test]
    fn inspects_frozen_v5_without_event_store_but_v6_requires_it() {
        // A v5 recovery point written before event-store capture (0427 voice,
        // 0428 presence, 0432 member deltas, 0430 message/reaction) declares
        // every covered table except the v6 addition and stays readable;
        // promoting it to v6 without the event store is refused.
        let tables: Vec<Value> = DUMP_TABLES
            .iter()
            .copied()
            .filter(|name| {
                !V6_ADDED_TABLES.contains(name)
                    && ![
                        "starboard_config",
                        "starboard_starrers",
                        "starboard_posts",
                        "giveaways",
                        "giveaway_entries",
                        "giveaway_draws",
                    ]
                    .contains(name)
            })
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "columns": [],
                    "column_types": [],
                    "count": 0,
                })
            })
            .collect();
        let mut v5 = manifest(tables);
        v5["version"] = serde_json::json!(5);
        assert!(
            validate_manifest(&v5).is_ok(),
            "frozen v5 without event_store stays readable"
        );
        let bytes = gzip_lines(&[v5.clone(), serde_json::json!({"kind":"end","rows":0})]);
        let contents = inspect_bytes(&bytes).expect("frozen v5 inspects");
        assert_eq!(contents.manifest.version, 5);
        // v6 still requires the event store; so does the current version.
        let mut v6 = v5.clone();
        v6["version"] = serde_json::json!(6);
        assert!(validate_manifest(&v6)
            .unwrap_err()
            .to_string()
            .contains("event_store"));
        let mut current = v5;
        current["version"] = serde_json::json!(DUMP_VERSION);
        assert_eq!(DUMP_VERSION, 15, "frozen v5 test pins the v15 bump");
        assert!(validate_manifest(&current)
            .unwrap_err()
            .to_string()
            .contains("event_store"));
    }

    #[test]
    fn v6_archives_read_without_seasonal_but_v7_requires_it() {
        fn without(tables: Vec<Value>, name: &str) -> Vec<Value> {
            tables
                .into_iter()
                .filter(|t| {
                    t.get("name").and_then(Value::as_str) != Some(name)
                        && ![
                            "starboard_config",
                            "starboard_starrers",
                            "starboard_posts",
                            "giveaways",
                            "giveaway_entries",
                            "giveaway_draws",
                        ]
                        .iter()
                        .any(|n| t["name"] == *n)
                })
                .collect()
        }
        // A v6 recovery point written before the seasonal XP projection (0100)
        // declares every covered table except member_season_xp and stays
        // readable; promoting it to v7 without the table is refused. This is
        // the frozen-manifest guard for the v7 bump (now pinned at v14):
        // building the old manifest from today's inventory (as the v5 tests
        // do) would silently include the new table and miss the refusal.
        let mut v6 = manifest(without(
            complete_tables(&BTreeMap::new()),
            "member_season_xp",
        ));
        v6["version"] = serde_json::json!(6);
        assert!(
            validate_manifest(&v6).is_ok(),
            "v6 without member_season_xp reads"
        );
        // The inspect read path accepts the frozen v6 envelope too.
        let bytes = gzip_lines(&[v6.clone(), serde_json::json!({"kind":"end","rows":0})]);
        let contents = inspect_bytes(&bytes).expect("v6 without member_season_xp inspects");
        assert_eq!(contents.manifest.version, 6);
        assert_eq!(
            contents.manifest.missing_tables(),
            [
                "member_season_xp",
                "starboard_config",
                "starboard_starrers",
                "starboard_posts",
                "giveaways",
                "giveaway_entries",
                "giveaway_draws"
            ]
        );
        // Dropping a v6-covered table from a v6 manifest still refuses.
        let mut v6_gap = v6.clone();
        v6_gap["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|t| t.get("name").and_then(Value::as_str) != Some("event_store"));
        assert!(validate_manifest(&v6_gap)
            .unwrap_err()
            .to_string()
            .contains("event_store"));
        // v7 without member_season_xp refuses.
        let v7_gap = manifest(without(
            complete_tables(&BTreeMap::new()),
            "member_season_xp",
        ));
        assert_eq!(v7_gap["version"], serde_json::json!(DUMP_VERSION));
        assert_eq!(DUMP_VERSION, 15, "frozen v6 test pins the v15 bump");
        assert!(validate_manifest(&v7_gap)
            .unwrap_err()
            .to_string()
            .contains("member_season_xp"));
    }

    #[test]
    fn v6_pre_retention_archives_stay_readable_but_v7_requires_retention() {
        fn without_any(tables: Vec<Value>, names: &[&str]) -> Vec<Value> {
            tables
                .into_iter()
                .filter(|t| {
                    t.get("name").and_then(Value::as_str).is_none_or(|n| {
                        !names.contains(&n)
                            && ![
                                "starboard_config",
                                "starboard_starrers",
                                "starboard_posts",
                                "giveaways",
                                "giveaway_entries",
                                "giveaway_draws",
                            ]
                            .contains(&n)
                    })
                })
                .collect()
        }
        // A v6 recovery point written on main after the event-store bump but
        // before the retention queues (0438) declares every covered table
        // except the two retention queues and the seasonal projection, and
        // stays readable; promoting it to v7 without the retention tables is
        // refused. This is the frozen-manifest guard for the v7 grouping:
        // building the old manifest from today's inventory via the V-arrays
        // would silently include the new tables and miss the refusal, so the
        // pre-PR inventory is pinned here by explicit table names.
        let pre_retention = without_any(
            complete_tables(&BTreeMap::new()),
            &[
                "member_season_xp",
                "retention_guild_purge_queue",
                "retention_deletion_requests",
            ],
        );
        let mut v6 = manifest(pre_retention);
        v6["version"] = serde_json::json!(6);
        assert!(
            validate_manifest(&v6).is_ok(),
            "v6 without the v7 retention queues reads"
        );
        // The inspect read path accepts the frozen pre-retention v6 envelope.
        let bytes = gzip_lines(&[v6.clone(), serde_json::json!({"kind":"end","rows":0})]);
        let contents = inspect_bytes(&bytes).expect("pre-retention v6 inspects");
        assert_eq!(contents.manifest.version, 6);
        assert_eq!(
            contents.manifest.missing_tables(),
            [
                "member_season_xp",
                "retention_guild_purge_queue",
                "retention_deletion_requests",
                "starboard_config",
                "starboard_starrers",
                "starboard_posts",
                "giveaways",
                "giveaway_entries",
                "giveaway_draws"
            ]
        );
        // Dropping the v6 event store from a v6 manifest still refuses.
        let mut v6_gap = v6.clone();
        v6_gap["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|t| t.get("name").and_then(Value::as_str) != Some("event_store"));
        assert!(validate_manifest(&v6_gap)
            .unwrap_err()
            .to_string()
            .contains("event_store"));
        // v7 without the retention queues refuses.
        let mut v7_gap = v6.clone();
        v7_gap["version"] = serde_json::json!(DUMP_VERSION);
        assert_eq!(
            DUMP_VERSION, 15,
            "frozen pre-retention test pins the v15 bump"
        );
        let err = validate_manifest(&v7_gap).unwrap_err().to_string();
        assert!(err.contains("retention_guild_purge_queue"), "{err}");
        assert!(err.contains("retention_deletion_requests"), "{err}");
    }

    #[test]
    fn v7_archives_read_without_stream_claims_but_v8_requires_them() {
        fn without(tables: Vec<Value>, name: &str) -> Vec<Value> {
            tables
                .into_iter()
                .filter(|t| {
                    t.get("name").and_then(Value::as_str) != Some(name)
                        && ![
                            "starboard_config",
                            "starboard_starrers",
                            "starboard_posts",
                            "giveaways",
                            "giveaway_entries",
                            "giveaway_draws",
                        ]
                        .iter()
                        .any(|n| t["name"] == *n)
                })
                .collect()
        }
        // A v7 recovery point written before the stream go-live ledger (0440/0443)
        // declares every covered table except stream_live_claims and stays
        // readable; promoting it to v8 without the table is refused. This is
        // the frozen-manifest guard for the v8 bump: building the old manifest
        // from today's inventory via the V-arrays would silently include the
        // new table and miss the refusal, so the pre-PR inventory is pinned
        // here by the explicit table name.
        let mut v7 = manifest(without(
            complete_tables(&BTreeMap::new()),
            "stream_live_claims",
        ));
        v7["version"] = serde_json::json!(7);
        assert!(
            validate_manifest(&v7).is_ok(),
            "v7 without stream_live_claims reads"
        );
        // The inspect read path accepts the frozen v7 envelope too.
        let bytes = gzip_lines(&[v7.clone(), serde_json::json!({"kind":"end","rows":0})]);
        let contents = inspect_bytes(&bytes).expect("v7 without stream_live_claims inspects");
        assert_eq!(contents.manifest.version, 7);
        assert_eq!(
            contents.manifest.missing_tables(),
            [
                "stream_live_claims",
                "starboard_config",
                "starboard_starrers",
                "starboard_posts",
                "giveaways",
                "giveaway_entries",
                "giveaway_draws"
            ]
        );
        // Dropping a v7-covered table from a v7 manifest still refuses.
        let mut v7_gap = v7.clone();
        v7_gap["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|t| t.get("name").and_then(Value::as_str) != Some("member_season_xp"));
        assert!(validate_manifest(&v7_gap)
            .unwrap_err()
            .to_string()
            .contains("member_season_xp"));
        // v8 without stream_live_claims refuses.
        let v8_gap = manifest(without(
            complete_tables(&BTreeMap::new()),
            "stream_live_claims",
        ));
        assert_eq!(v8_gap["version"], serde_json::json!(DUMP_VERSION));
        assert_eq!(DUMP_VERSION, 15, "frozen v7 test pins the v15 bump");
        assert!(validate_manifest(&v8_gap)
            .unwrap_err()
            .to_string()
            .contains("stream_live_claims"));
    }

    #[test]
    fn v8_archives_read_without_guild_counters_but_v9_requires_them() {
        // Literal names, not V9_ADDED_TABLES: this reproduces the pre-change
        // inventory even if the version waiver is accidentally removed.
        let tables = complete_tables(&BTreeMap::new())
            .into_iter()
            .filter(|table| {
                table["name"] != "assistant_monthly_builds"
                    && table["name"] != "game_activity_guild_aggregates"
                    && table["name"] != "starboard_config"
                    && table["name"] != "starboard_starrers"
                    && table["name"] != "starboard_posts"
                    && table["name"] != "giveaways"
                    && table["name"] != "giveaway_entries"
                    && table["name"] != "giveaway_draws"
            })
            .collect();
        let mut old = manifest(tables);
        for version in 4..=8 {
            old["version"] = serde_json::json!(version);
            let contents = inspect_bytes(&gzip_lines(&[
                old.clone(),
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .expect("old archives without the v9 guild counters stay readable");
            assert_eq!(contents.manifest.version, version);
            assert_eq!(
                contents.manifest.missing_tables(),
                [
                    "game_activity_guild_aggregates",
                    "assistant_monthly_builds",
                    "starboard_config",
                    "starboard_starrers",
                    "starboard_posts",
                    "giveaways",
                    "giveaway_entries",
                    "giveaway_draws"
                ]
            );
            let mut gap = old.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != "events");
            assert!(validate_manifest(&gap)
                .unwrap_err()
                .to_string()
                .contains("missing tables: events"));
        }
        assert_eq!(DUMP_VERSION, 15, "new dumps require both guild counters");
        let current = manifest(complete_tables(&BTreeMap::new()));
        inspect_bytes(&gzip_lines(&[
            current.clone(),
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .expect("complete v15 archive inspects");
        for name in ["assistant_monthly_builds", "game_activity_guild_aggregates"] {
            let mut gap = current.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != name);
            let error = inspect_bytes(&gzip_lines(&[
                gap,
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .unwrap_err()
            .to_string();
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn v9_archives_read_without_epoch_claim_but_v10_requires_it() {
        // Literal name, not V10_ADDED_TABLES: this reproduces the pre-change
        // inventory even if the version waiver is accidentally removed.
        let tables = complete_tables(&BTreeMap::new())
            .into_iter()
            .filter(|table| {
                table["name"] != "scheduled_events_claim"
                    && ![
                        "starboard_config",
                        "starboard_starrers",
                        "starboard_posts",
                        "giveaways",
                        "giveaway_entries",
                        "giveaway_draws",
                    ]
                    .iter()
                    .any(|n| table["name"] == *n)
            })
            .collect();
        let mut old = manifest(tables);
        for version in 4..=9 {
            old["version"] = serde_json::json!(version);
            let contents = inspect_bytes(&gzip_lines(&[
                old.clone(),
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .expect("old archives without the v10 epoch-claim guard stay readable");
            assert_eq!(contents.manifest.version, version);
            assert_eq!(
                contents.manifest.missing_tables(),
                [
                    "scheduled_events_claim",
                    "starboard_config",
                    "starboard_starrers",
                    "starboard_posts",
                    "giveaways",
                    "giveaway_entries",
                    "giveaway_draws"
                ]
            );
            let mut gap = old.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != "events");
            assert!(validate_manifest(&gap)
                .unwrap_err()
                .to_string()
                .contains("missing tables: events"));
        }
        assert_eq!(DUMP_VERSION, 15, "new dumps require the epoch-claim guard");
        let current = manifest(complete_tables(&BTreeMap::new()));
        inspect_bytes(&gzip_lines(&[
            current.clone(),
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .expect("complete v15 archive inspects");
        let mut gap = current.clone();
        gap["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|table| table["name"] != "scheduled_events_claim");
        let error = inspect_bytes(&gzip_lines(&[
            gap,
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .unwrap_err()
        .to_string();
        assert!(error.contains("scheduled_events_claim"), "{error}");
    }

    #[test]
    fn v10_archives_read_without_tombstone_but_v11_requires_it() {
        // Literal names, not V11_ADDED_TABLES: this reproduces the pre-change
        // inventory even if the version waiver is accidentally removed. This
        // is the frozen claim-only v10 shape written by the merged v10 writer:
        // the epoch-claim guard is present, the delete tombstones and v13
        // event voice pair, v12 registry and v14 starboard tables are absent.
        let tables = complete_tables(&BTreeMap::new())
            .into_iter()
            .filter(|table| {
                table["name"] != "scheduled_events_tombstone"
                    && table["name"] != "event_voice_channels"
                    && table["name"] != "event_voice_presence"
                    && table["name"] != "starboard_config"
                    && table["name"] != "starboard_starrers"
                    && table["name"] != "starboard_posts"
                    && table["name"] != "giveaways"
                    && table["name"] != "giveaway_entries"
                    && table["name"] != "giveaway_draws"
                    && table["name"] != "admin_api_tokens"
            })
            .collect();
        let mut old = manifest(tables);
        for version in 4..=10 {
            old["version"] = serde_json::json!(version);
            let contents = inspect_bytes(&gzip_lines(&[
                old.clone(),
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .expect("old archives without the v11 delete tombstones stay readable");
            assert_eq!(contents.manifest.version, version);
            assert_eq!(
                contents.manifest.missing_tables(),
                [
                    "scheduled_events_tombstone",
                    "event_voice_channels",
                    "event_voice_presence",
                    "admin_api_tokens",
                    "starboard_config",
                    "starboard_starrers",
                    "starboard_posts",
                    "giveaways",
                    "giveaway_entries",
                    "giveaway_draws"
                ]
            );
            let mut gap = old.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != "events");
            assert!(validate_manifest(&gap)
                .unwrap_err()
                .to_string()
                .contains("missing tables: events"));
        }
        assert_eq!(DUMP_VERSION, 15, "new dumps require the delete tombstones");
        // v11 kept the tombstones mandatory but predates registry and starboard:
        // the frozen old-v11 shape carries the tombstone and omits the
        // registry, event voice pair and starboard three.
        assert_eq!(DUMP_VERSION, 15, "new dumps require the v11 tombstones");
        let old_v11: Vec<Value> = complete_tables(&BTreeMap::new())
            .into_iter()
            .filter(|table| {
                table["name"] != "admin_api_tokens"
                    && table["name"] != "event_voice_channels"
                    && table["name"] != "event_voice_presence"
                    && table["name"] != "starboard_config"
                    && table["name"] != "starboard_starrers"
                    && table["name"] != "starboard_posts"
                    && table["name"] != "giveaways"
                    && table["name"] != "giveaway_entries"
                    && table["name"] != "giveaway_draws"
            })
            .collect();
        let mut at_v11 = manifest(old_v11);
        at_v11["version"] = serde_json::json!(11);
        let contents = inspect_bytes(&gzip_lines(&[
            at_v11.clone(),
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .expect("old v11 archives without the starboard tables stay readable");
        assert_eq!(contents.manifest.version, 11);
        assert_eq!(
            contents.manifest.missing_tables(),
            [
                "event_voice_channels",
                "event_voice_presence",
                "admin_api_tokens",
                "starboard_config",
                "starboard_starrers",
                "starboard_posts",
                "giveaways",
                "giveaway_entries",
                "giveaway_draws"
            ]
        );
        let mut tombstone_gap = at_v11.clone();
        tombstone_gap["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|table| table["name"] != "scheduled_events_tombstone");
        assert!(validate_manifest(&tombstone_gap)
            .unwrap_err()
            .to_string()
            .contains("scheduled_events_tombstone"));
        let current = manifest(complete_tables(&BTreeMap::new()));
        inspect_bytes(&gzip_lines(&[
            current.clone(),
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .expect("complete v15 archive inspects");
        let mut gap = current.clone();
        gap["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|table| table["name"] != "scheduled_events_tombstone");
        let error = inspect_bytes(&gzip_lines(&[
            gap,
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .unwrap_err()
        .to_string();
        assert!(error.contains("scheduled_events_tombstone"), "{error}");
    }

    #[test]
    fn v11_archives_read_without_registry_but_v12_requires_it() {
        // Literal name, not V12_ADDED_TABLES: this reproduces the pre-change
        // inventory even if the version waiver is accidentally removed. This
        // is the frozen tombstone-only v11 shape written by the merged v11
        // writer: the delete tombstones are present, the token registry is
        // absent, as are the v13 voice pair and v14 starboard tables.
        let tables = complete_tables(&BTreeMap::new())
            .into_iter()
            .filter(|table| {
                table["name"] != "admin_api_tokens"
                    && table["name"] != "event_voice_channels"
                    && table["name"] != "event_voice_presence"
                    && table["name"] != "starboard_config"
                    && table["name"] != "starboard_starrers"
                    && table["name"] != "starboard_posts"
                    && table["name"] != "giveaways"
                    && table["name"] != "giveaway_entries"
                    && table["name"] != "giveaway_draws"
            })
            .collect();
        let mut old = manifest(tables);
        for version in 4..=11 {
            old["version"] = serde_json::json!(version);
            let contents = inspect_bytes(&gzip_lines(&[
                old.clone(),
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .expect("old archives without the v12 token registry stay readable");
            assert_eq!(contents.manifest.version, version);
            assert_eq!(
                contents.manifest.missing_tables(),
                [
                    "event_voice_channels",
                    "event_voice_presence",
                    "admin_api_tokens",
                    "starboard_config",
                    "starboard_starrers",
                    "starboard_posts",
                    "giveaways",
                    "giveaway_entries",
                    "giveaway_draws"
                ]
            );
            let mut gap = old.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != "events");
            assert!(validate_manifest(&gap)
                .unwrap_err()
                .to_string()
                .contains("missing tables: events"));
        }
        let mut v12_gap = old.clone();
        v12_gap["version"] = serde_json::json!(12);
        assert!(validate_manifest(&v12_gap)
            .unwrap_err()
            .to_string()
            .contains("admin_api_tokens"));
        assert_eq!(DUMP_VERSION, 15, "new dumps require the token registry");
        let current = manifest(complete_tables(&BTreeMap::new()));
        inspect_bytes(&gzip_lines(&[
            current.clone(),
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .expect("complete v15 archive inspects");
        let mut gap = current.clone();
        gap["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|table| table["name"] != "admin_api_tokens");
        let error = inspect_bytes(&gzip_lines(&[
            gap,
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .unwrap_err()
        .to_string();
        assert!(error.contains("admin_api_tokens"), "{error}");
    }

    #[test]
    fn v12_archives_read_without_event_voice_but_v13_requires_it() {
        // Literal names, not V13_ADDED_TABLES: this reproduces the pre-change
        // inventory even if the version waiver is accidentally removed.
        let tables = complete_tables(&BTreeMap::new())
            .into_iter()
            .filter(|table| {
                table["name"] != "event_voice_channels"
                    && table["name"] != "event_voice_presence"
                    && table["name"] != "starboard_config"
                    && table["name"] != "starboard_starrers"
                    && table["name"] != "starboard_posts"
                    && table["name"] != "giveaways"
                    && table["name"] != "giveaway_entries"
                    && table["name"] != "giveaway_draws"
            })
            .collect();
        let mut old = manifest(tables);
        for version in 4..=12 {
            old["version"] = serde_json::json!(version);
            let contents = inspect_bytes(&gzip_lines(&[
                old.clone(),
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .expect("old archives without the v13 event voice pair stay readable");
            assert_eq!(contents.manifest.version, version);
            assert_eq!(
                contents.manifest.missing_tables(),
                [
                    "event_voice_channels",
                    "event_voice_presence",
                    "starboard_config",
                    "starboard_starrers",
                    "starboard_posts",
                    "giveaways",
                    "giveaway_entries",
                    "giveaway_draws"
                ]
            );
            let mut gap = old.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != "events");
            assert!(validate_manifest(&gap)
                .unwrap_err()
                .to_string()
                .contains("missing tables: events"));
        }
        assert_eq!(DUMP_VERSION, 15, "new dumps require the event voice pair");
        let current = manifest(complete_tables(&BTreeMap::new()));
        inspect_bytes(&gzip_lines(&[
            current.clone(),
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .expect("complete v15 archive inspects");
        for name in ["event_voice_channels", "event_voice_presence"] {
            let mut gap = current.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != name);
            let error = inspect_bytes(&gzip_lines(&[
                gap,
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .unwrap_err()
            .to_string();
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn v12_archives_read_without_starboard_but_v14_requires_each_table() {
        let tables = FROZEN_V12_TABLES.iter().map(|name| {
            serde_json::json!({"name": name, "columns": [], "column_types": [], "count": 0})
        }).collect();
        let mut old = manifest(tables);
        for version in 4..=12 {
            old["version"] = serde_json::json!(version);
            let contents = inspect_bytes(&gzip_lines(&[
                old.clone(),
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .expect("frozen registry-present v12 coverage stays readable");
            assert_eq!(contents.manifest.version, version);
            assert_eq!(contents.manifest.tables.len(), 99);
            assert_eq!(
                contents.manifest.missing_tables(),
                [
                    "event_voice_channels",
                    "event_voice_presence",
                    "starboard_config",
                    "starboard_starrers",
                    "starboard_posts",
                    "giveaways",
                    "giveaway_entries",
                    "giveaway_draws"
                ]
            );
            let mut gap = old.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != "events");
            assert!(validate_manifest(&gap)
                .unwrap_err()
                .to_string()
                .contains("missing tables: events"));
        }
        let mut missing_registry = old;
        missing_registry["version"] = serde_json::json!(12);
        missing_registry["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|table| table["name"] != "admin_api_tokens");
        let error = inspect_bytes(&gzip_lines(&[
            missing_registry,
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .unwrap_err()
        .to_string();
        assert!(error.contains("admin_api_tokens"), "{error}");
        let released_without_voice: Vec<_> = FROZEN_V13_TABLES
            .iter()
            .copied()
            .filter(|name| !["event_voice_channels", "event_voice_presence"].contains(name))
            .collect();
        assert_eq!(released_without_voice, FROZEN_V12_TABLES);
        assert_eq!(
            DUMP_VERSION, 15,
            "new dumps require the v14 starboard tables"
        );
        assert_eq!(
            DUMP_TABLES.len(),
            107,
            "released 101 plus three starboard and three giveaway tables"
        );
        assert_eq!(&DUMP_TABLES[..101], FROZEN_V13_TABLES.as_slice());
        assert_eq!(
            &DUMP_TABLES[101..],
            [
                "starboard_config",
                "starboard_starrers",
                "starboard_posts",
                "giveaways",
                "giveaway_entries",
                "giveaway_draws"
            ]
        );
        let current = manifest(complete_tables(&BTreeMap::new()));
        inspect_bytes(&gzip_lines(&[
            current.clone(),
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .expect("complete v15 archive inspects");
        for name in [
            "starboard_config",
            "starboard_starrers",
            "starboard_posts",
            "giveaways",
            "giveaway_entries",
            "giveaway_draws",
            "event_voice_channels",
            "event_voice_presence",
            "admin_api_tokens",
        ] {
            let mut gap = current.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != name);
            let error = inspect_bytes(&gzip_lines(&[
                gap,
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .unwrap_err()
            .to_string();
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn frozen_v13_inspects_without_starboard_and_requires_voice_and_registry() {
        // This literal released inventory does not depend on current DUMP_TABLES
        // or version-waiver constants: relabeling v13 as starboard must fail.
        let tables = FROZEN_V13_TABLES.iter().map(|name| {
            serde_json::json!({"name": name, "columns": [], "column_types": [], "count": 0})
        }).collect();
        let mut old = manifest(tables);
        old["version"] = serde_json::json!(13);
        let contents = inspect_bytes(&gzip_lines(&[
            old.clone(),
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .expect("released voice-and-registry v13 archive remains readable without starboard");
        assert_eq!(contents.manifest.version, 13);
        assert_eq!(contents.manifest.tables.len(), 101);
        assert_eq!(
            contents.manifest.missing_tables(),
            [
                "starboard_config",
                "starboard_starrers",
                "starboard_posts",
                "giveaways",
                "giveaway_entries",
                "giveaway_draws"
            ]
        );
        assert_eq!(
            V13_ADDED_TABLES,
            ["event_voice_channels", "event_voice_presence"]
        );
        for name in [
            "event_voice_channels",
            "event_voice_presence",
            "admin_api_tokens",
            "events",
        ] {
            let mut gap = old.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != name);
            let error = inspect_bytes(&gzip_lines(&[
                gap,
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .unwrap_err()
            .to_string();
            assert!(
                error.contains(name),
                "v13 must still require {name}: {error}"
            );
        }
        let mut mislabeled = old;
        mislabeled["version"] = serde_json::json!(14);
        let error = inspect_bytes(&gzip_lines(&[
            mislabeled,
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .unwrap_err()
        .to_string();
        for name in ["starboard_config", "starboard_starrers", "starboard_posts"] {
            assert!(error.contains(name), "v14 must require {name}: {error}");
        }
    }

    #[test]
    fn v14_archives_read_without_giveaways_but_v15_requires_them() {
        assert_eq!(
            DUMP_VERSION, 15,
            "giveaways follow the released v14 starboard format"
        );
        // Literal names, not V15_ADDED_TABLES: this reproduces the pre-change
        // inventory even if the version waiver is accidentally removed.
        let without_giveaways = |tables: Vec<Value>| {
            tables
                .into_iter()
                .filter(|table| {
                    !["giveaways", "giveaway_entries", "giveaway_draws"]
                        .contains(&table["name"].as_str().unwrap_or_default())
                })
                .collect::<Vec<_>>()
        };
        let tables = without_giveaways(complete_tables(&BTreeMap::new()));
        let mut old = manifest(tables);
        for version in 4..=14 {
            old["version"] = serde_json::json!(version);
            let contents = inspect_bytes(&gzip_lines(&[
                old.clone(),
                serde_json::json!({"kind":"end","rows":0}),
            ]))
            .expect("old archives without the v15 giveaway tables stay readable");
            assert_eq!(contents.manifest.version, version);
            assert_eq!(
                contents.manifest.missing_tables(),
                ["giveaways", "giveaway_entries", "giveaway_draws"]
            );
            let mut gap = old.clone();
            gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != "events");
            assert!(validate_manifest(&gap)
                .unwrap_err()
                .to_string()
                .contains("missing tables: events"));
        }
        let mut v12_registry_gap = old.clone();
        v12_registry_gap["version"] = serde_json::json!(12);
        v12_registry_gap["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|table| table["name"] != "admin_api_tokens");
        assert!(validate_manifest(&v12_registry_gap)
            .unwrap_err()
            .to_string()
            .contains("missing tables: admin_api_tokens"));
        for name in [
            "event_voice_channels",
            "event_voice_presence",
            "starboard_config",
            "starboard_starrers",
            "starboard_posts",
            "admin_api_tokens",
        ] {
            let mut v14_gap = old.clone();
            v14_gap["tables"]
                .as_array_mut()
                .unwrap()
                .retain(|table| table["name"] != name);
            let error = validate_manifest(&v14_gap).unwrap_err().to_string();
            assert!(error.contains(name), "{error}");
        }
        assert_eq!(DUMP_VERSION, 15, "new dumps require the giveaway tables");
        let current = manifest(complete_tables(&BTreeMap::new()));
        inspect_bytes(&gzip_lines(&[
            current.clone(),
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .expect("complete v15 archive inspects");
        let mut gap = current.clone();
        gap["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|table| table["name"] != "giveaways");
        let error = inspect_bytes(&gzip_lines(&[
            gap,
            serde_json::json!({"kind":"end","rows":0}),
        ]))
        .unwrap_err()
        .to_string();
        assert!(error.contains("giveaways"), "{error}");
    }

    #[test]
    fn v5_archives_read_without_event_store_but_v6_requires_it() {
        fn without(tables: Vec<Value>, name: &str) -> Vec<Value> {
            tables
                .into_iter()
                .filter(|t| {
                    t.get("name").and_then(Value::as_str) != Some(name)
                        && ![
                            "starboard_config",
                            "starboard_starrers",
                            "starboard_posts",
                            "giveaways",
                            "giveaway_entries",
                            "giveaway_draws",
                        ]
                        .iter()
                        .any(|n| t["name"] == *n)
                })
                .collect()
        }
        // A v5 manifest without event_store stays readable: v5 predates
        // event-store capture (voice, presence, member deltas, thread
        // lifecycle, message/reaction, stage/interaction/audit), and the v5
        // waiver covers every v6 table.
        let mut v5 = manifest(without(complete_tables(&BTreeMap::new()), "event_store"));
        v5["version"] = serde_json::json!(5);
        assert!(
            validate_manifest(&v5).is_ok(),
            "v5 without event_store reads"
        );
        // The inspect read path accepts the frozen v5 envelope too.
        let bytes = gzip_lines(&[v5.clone(), serde_json::json!({"kind":"end","rows":0})]);
        let contents = inspect_bytes(&bytes).expect("v5 without event_store inspects");
        assert_eq!(contents.manifest.version, 5);
        assert_eq!(
            contents.manifest.missing_tables(),
            [
                "event_store",
                "starboard_config",
                "starboard_starrers",
                "starboard_posts",
                "giveaways",
                "giveaway_entries",
                "giveaway_draws"
            ]
        );
        // Dropping a v5-covered table from a v5 manifest still refuses.
        let mut v5_gap = v5.clone();
        v5_gap["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|t| t.get("name").and_then(Value::as_str) != Some("voice_create_reservations"));
        assert!(validate_manifest(&v5_gap)
            .unwrap_err()
            .to_string()
            .contains("voice_create_reservations"));
        // The current version without event_store refuses.
        let current_gap = manifest(without(complete_tables(&BTreeMap::new()), "event_store"));
        assert_eq!(current_gap["version"], serde_json::json!(DUMP_VERSION));
        assert!(validate_manifest(&current_gap)
            .unwrap_err()
            .to_string()
            .contains("event_store"));
    }

    #[test]
    fn inspect_still_reads_v5_archives_and_refuses_unknown_versions() {
        // The version gate in `inspect_reader` is a second half of the v8
        // bump: `validate_manifest` waives newer tables for v5, but the gate
        // must also let a v5 archive reach it.
        let mut v5 = good_dump();
        v5[0]["version"] = serde_json::json!(5);
        v5[0]["tables"].as_array_mut().unwrap().retain(|t| {
            t.get("name").and_then(Value::as_str) != Some("event_store")
                && ![
                    "starboard_config",
                    "starboard_starrers",
                    "starboard_posts",
                    "giveaways",
                    "giveaway_entries",
                    "giveaway_draws",
                ]
                .iter()
                .any(|n| t["name"] == *n)
        });
        let contents = inspect_bytes(&gzip_lines(&v5)).expect("a v5 archive stays readable");
        assert_eq!(contents.manifest.version, 5);
        assert_eq!(contents.rows, 1);
        assert_eq!(
            contents.manifest.missing_tables(),
            [
                "event_store",
                "starboard_config",
                "starboard_starrers",
                "starboard_posts",
                "giveaways",
                "giveaway_entries",
                "giveaway_draws"
            ]
        );

        let mut future = good_dump();
        future[0]["version"] = serde_json::json!(DUMP_VERSION + 1);
        let message = inspect_bytes(&gzip_lines(&future)).unwrap_err().to_string();
        assert!(
            message.contains(&format!(
                "reads 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14 and {DUMP_VERSION}"
            )),
            "unknown versions are refused with the readable list: {message}"
        );
    }

    #[test]
    fn v3_requires_its_original_tables_but_v4_requires_its_frozen_coverage() {
        let legacy: Value = serde_json::from_str(
            include_str!("../../tests/fixtures/legacy-v3-native.ndjson")
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        let mut missing_v3 = legacy.clone();
        missing_v3["tables"].as_array_mut().unwrap().remove(0);
        assert!(validate_manifest(&missing_v3)
            .unwrap_err()
            .to_string()
            .contains("missing tables"));
        let mut incomplete_v4 = legacy;
        incomplete_v4["version"] = serde_json::json!(4);
        assert!(validate_manifest(&incomplete_v4)
            .unwrap_err()
            .to_string()
            .contains("missing tables"));
    }

    #[test]
    fn refuses_a_native_cell_in_a_text_encoded_dump() {
        // Native cells are valid in legacy dumps, not in this port's
        // explicitly marked PostgreSQL text-output encoding.
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id"], 1));
        let bytes = gzip_lines(&[
            manifest(complete_tables(&over)),
            serde_json::json!({"kind":"row","table":"events","data":{"id": 1}}),
            serde_json::json!({"kind":"end","rows":1}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("native cell must be refused");
        assert!(err.to_string().contains("not a string or null"), "{err}");
    }

    #[test]
    fn legacy_cells_decode_using_target_types_without_null_coercion() {
        for (value, ty, expected) in [
            (serde_json::json!(42), "bigint", "42"),
            (serde_json::json!(false), "boolean", "false"),
            (serde_json::json!(true), "boolean", "true"),
            (serde_json::json!("{\"k\":1}"), "text", "{\"k\":1}"),
            (serde_json::json!("hello"), "jsonb", "\"hello\""),
            (serde_json::json!({"k":1}), "jsonb", "{\"k\":1}"),
            (serde_json::json!([true, 2]), "json", "[true,2]"),
        ] {
            assert_eq!(
                cell_input(&value, ty, false).unwrap().as_deref(),
                Some(expected)
            );
        }
        assert_eq!(cell_input(&Value::Null, "text", false).unwrap(), None);
        assert_eq!(
            cell_input(&serde_json::json!("{\"k\":1}"), "jsonb", true)
                .unwrap()
                .as_deref(),
            Some("{\"k\":1}")
        );
        assert!(cell_input(&serde_json::json!({"k":1}), "text", false).is_err());
        assert!(cell_input(&serde_json::json!(42), "bigint", true).is_err());
    }

    #[test]
    fn caps_a_highly_compressible_line_before_unbounded_allocation() {
        let mut enc = new_encoder();
        enc.write_all(&vec![b' '; 32 * 1024]).unwrap();
        let bytes = finish_gzip(enc).unwrap();
        assert!(bytes.len() < 1024);
        let limits = InspectLimits {
            line: 1024,
            ..INSPECT_LIMITS
        };
        let err = inspect_reader(bytes.as_slice(), limits).unwrap_err();
        assert!(err.to_string().contains("decoded line exceeds"), "{err}");
    }

    #[test]
    fn caps_cumulative_decoded_bytes_even_when_each_line_is_small() {
        let mut enc = new_encoder();
        enc.write_all(&vec![b'\n'; 32 * 1024]).unwrap();
        let bytes = finish_gzip(enc).unwrap();
        assert!(bytes.len() < 1024);
        let limits = InspectLimits {
            decoded: 1024,
            ..INSPECT_LIMITS
        };
        let err = inspect_reader(bytes.as_slice(), limits).unwrap_err();
        assert!(err.to_string().contains("decoded content exceeds"), "{err}");
    }

    #[test]
    fn caps_cumulative_retained_rows_including_value_overhead() {
        let mut objs = good_dump();
        objs[0]["tables"][0]["count"] = serde_json::json!(2);
        objs.insert(2, objs[1].clone());
        objs[3]["rows"] = serde_json::json!(2);
        let weight = retained_weight(&objs[0]) + 2 * retained_weight(&objs[1]);
        let bytes = gzip_lines(&objs);
        inspect_reader(
            bytes.as_slice(),
            InspectLimits {
                retained: weight,
                ..INSPECT_LIMITS
            },
        )
        .unwrap();
        let err = inspect_reader(
            bytes.as_slice(),
            InspectLimits {
                retained: weight - 1,
                ..INSPECT_LIMITS
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("retained data exceeds"), "{err}");
    }

    #[test]
    fn size_cap_refuses_past_the_limit_and_accepts_at_it() {
        ensure_within_size_cap(MAX_DUMP_BYTES, "dump").expect("at cap is fine");
        let err = ensure_within_size_cap(MAX_DUMP_BYTES + 1, "dump").expect_err("past cap refused");
        assert!(err.to_string().contains("past the"), "{err}");
    }

    #[test]
    fn inspect_refuses_an_oversize_file_from_metadata_without_reading_it() {
        // Sparse file: the length is metadata, no bytes are allocated, and
        // `inspect` refuses from the metadata before `fs::read` runs.
        let dir = publication_dir("reader-metadata");
        let path = dir.join("oversize.ndjson.gz");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_DUMP_BYTES + 1)
            .unwrap();
        let err = inspect(&path).expect_err("oversize must be refused");
        assert!(err.to_string().contains("past the"), "{err}");
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn refuses_a_row_before_the_manifest() {
        let bytes = gzip_lines(&[
            serde_json::json!({"kind":"row","table":"events","data":{}}),
            manifest(complete_tables(&BTreeMap::new())),
            serde_json::json!({"kind":"end","rows":0}),
        ]);
        let err = inspect_bytes(&bytes).expect_err("row before manifest");
        assert!(err.to_string().contains("before the manifest"), "{err}");
    }

    // Acceptance corpus: dump-inspect refusal shapes. The duplicate/missing/
    // undeclared/non-object cases pin refusals the reader already makes; the
    // column-parity and row-key cases are the new inspect checks. Every
    // refusal must use fixed vocabulary: a sentinel string that only exists
    // inside the hostile archive must never appear in the error, so an
    // archive cannot smuggle its own contents into operator output.
    const ECHO_SENTINEL: &str = "sentinel_archive_echo_7c1d";

    fn acceptance_refusal(objs: &[Value]) -> String {
        inspect_bytes(&gzip_lines(objs)).unwrap_err().to_string()
    }

    fn acceptance_table_mut<'a>(manifest: &'a mut Value, name: &str) -> &'a mut Value {
        manifest["tables"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|table| table.get("name").and_then(Value::as_str) == Some(name))
            .unwrap()
    }

    #[test]
    fn acceptance_dump_inspect_refuses_duplicate_manifest_line() {
        let mut objs = empty_dump();
        let mut second = objs[0].clone();
        second[ECHO_SENTINEL] = serde_json::json!(ECHO_SENTINEL);
        objs.insert(1, second);
        let err = acceptance_refusal(&objs);
        assert!(err.contains("more than one manifest"), "{err}");
        assert!(!err.contains(ECHO_SENTINEL), "{err}");
    }

    #[test]
    fn acceptance_dump_inspect_refuses_duplicated_manifest_table() {
        let mut objs = empty_dump();
        let mut duplicate = objs[0]["tables"][0].clone();
        duplicate["columns"] = serde_json::json!([ECHO_SENTINEL]);
        objs[0]["tables"].as_array_mut().unwrap().push(duplicate);
        let err = acceptance_refusal(&objs);
        assert!(err.contains("is duplicated"), "{err}");
        assert!(!err.contains(ECHO_SENTINEL), "{err}");
    }

    #[test]
    fn acceptance_dump_inspect_refuses_missing_manifest() {
        // An end marker alone and a wholly empty archive both lack the one
        // line every other check keys off.
        for objs in [vec![serde_json::json!({"kind":"end","rows":0})], Vec::new()] {
            let err = acceptance_refusal(&objs);
            assert!(err.contains("no manifest"), "{err}");
            assert!(!err.contains(ECHO_SENTINEL), "{err}");
        }
    }

    #[test]
    fn acceptance_dump_inspect_refuses_row_for_undeclared_table() {
        // A v5 manifest legitimately omits event_store, so a row naming it
        // is a row for a real dump table this manifest never declared.
        let mut objs = good_dump();
        objs[0]["version"] = serde_json::json!(5);
        objs[0]["tables"]
            .as_array_mut()
            .unwrap()
            .retain(|table| table.get("name").and_then(Value::as_str) != Some("event_store"));
        objs.insert(
            2,
            serde_json::json!({
                "kind": "row",
                "table": "event_store",
                "data": {"id": ECHO_SENTINEL},
            }),
        );
        let err = acceptance_refusal(&objs);
        assert!(err.contains("not declared in the manifest"), "{err}");
        assert!(!err.contains(ECHO_SENTINEL), "{err}");
    }

    #[test]
    fn acceptance_dump_inspect_refuses_non_object_tables_entry() {
        for entry in [
            serde_json::json!(ECHO_SENTINEL),
            serde_json::json!(7),
            serde_json::json!([]),
            serde_json::json!(true),
            Value::Null,
        ] {
            let mut objs = empty_dump();
            objs[0]["tables"][0] = entry.clone();
            let err = acceptance_refusal(&objs);
            assert!(
                err.contains("manifest table is not an object"),
                "{entry}: {err}"
            );
            assert!(!err.contains(ECHO_SENTINEL), "{entry}: {err}");
        }
    }

    #[test]
    fn acceptance_dump_inspect_refuses_column_types_length_mismatch() {
        // column_types is parallel to columns: a non-empty type list whose
        // length differs (either direction, including types for no columns)
        // leaves the restore cast for some column ambiguous.
        for (columns, types) in [
            (vec!["id", "guild_id"], vec!["text"]),
            (vec!["id", "guild_id"], vec!["text", "text", ECHO_SENTINEL]),
            (vec!["id"], vec!["text", ECHO_SENTINEL]),
            (Vec::new(), vec![ECHO_SENTINEL]),
        ] {
            let mut objs = empty_dump();
            let events = acceptance_table_mut(&mut objs[0], "events");
            events["columns"] = serde_json::json!(columns);
            events["column_types"] = serde_json::json!(types);
            let err = acceptance_refusal(&objs);
            assert!(err.to_lowercase().contains("column"), "{err}");
            assert!(!err.contains(ECHO_SENTINEL), "{err}");
        }
    }

    #[test]
    fn acceptance_dump_inspect_refuses_row_keys_outside_manifest_columns() {
        // The manifest column list is the restore contract: a row key it
        // never declared must refuse at inspect, typed or legacy-untyped.
        for strip_types in [false, true] {
            let mut objs = good_dump();
            if strip_types {
                for table in objs[0]["tables"].as_array_mut().unwrap() {
                    table.as_object_mut().unwrap().shift_remove("column_types");
                }
            }
            objs[1]["data"][ECHO_SENTINEL] = serde_json::json!(ECHO_SENTINEL);
            let err = acceptance_refusal(&objs);
            assert!(err.to_lowercase().contains("column"), "{err}");
            assert!(!err.contains(ECHO_SENTINEL), "{err}");
        }
    }

    #[test]
    fn acceptance_dump_inspect_accepts_valid_archive_unchanged() {
        let objs = good_dump();
        let contents = inspect_bytes(&gzip_lines(&objs)).unwrap();
        assert_eq!(contents.rows, 1);
        assert_eq!(
            serde_json::to_value(&contents.manifest).unwrap(),
            objs[0],
            "a valid manifest must round-trip byte-identically in value terms"
        );
        assert_eq!(contents.buffers.len(), 1);
        assert_eq!(
            contents.buffers["events"],
            vec![{
                let mut row = Map::new();
                row.insert("id".to_owned(), serde_json::json!("1"));
                row.insert("guild_id".to_owned(), serde_json::json!("g"));
                row
            }]
        );
    }

    #[test]
    fn duplicate_heavy_columns_preserve_row_completeness_verdicts() {
        // Duplicate declarations must not multiply the work for each row.
        // Keep this shape large enough to exercise the amplification without
        // a wall-clock assertion that would flake on a loaded CI runner.
        let row_count = 2_000;
        let mut over = BTreeMap::new();
        over.insert("events", (vec!["id"; 200_000], row_count));
        let mut objs = vec![manifest(complete_tables(&over))];
        let row = serde_json::json!({"kind":"row","table":"events","data":{"id":"1"}});
        objs.extend(std::iter::repeat_n(row, row_count as usize));
        objs.push(serde_json::json!({"kind":"end","rows":row_count}));
        let bytes = gzip_lines(&objs);
        let contents = inspect_bytes(&bytes).unwrap();
        assert_eq!(contents.rows, row_count);
        assert_eq!(contents.buffers["events"].len(), row_count as usize);
        assert_eq!(
            contents.buffers["events"][0],
            serde_json::json!({"id":"1"}).as_object().unwrap().clone()
        );

        // The same distinct-column contract applies to typed and legacy rows.
        for strip_types in [false, true] {
            if strip_types {
                objs[0]["version"] = serde_json::json!(3);
                for table in objs[0]["tables"].as_array_mut().unwrap() {
                    table.as_object_mut().unwrap().shift_remove("column_types");
                }
            }
            objs[1]["data"] = serde_json::json!({});
            let err = acceptance_refusal(&objs);
            assert!(err.contains("is missing a manifest column"), "{err}");
            // Equal key counts alone do not prove completeness: an outside
            // key replacing a required key must retain the earlier refusal.
            objs[1]["data"] = serde_json::json!({ECHO_SENTINEL:ECHO_SENTINEL});
            let err = acceptance_refusal(&objs);
            assert!(err.contains("has a column outside the manifest"), "{err}");
            assert!(!err.contains(ECHO_SENTINEL), "{err}");
            objs[1]["data"] = serde_json::json!({"id":"1"});
            assert_eq!(inspect_bytes(&gzip_lines(&objs)).unwrap().rows, row_count);
        }
    }

    #[test]
    fn acceptance_dump_inspect_refuses_row_missing_a_manifest_column() {
        // Completeness is the restore contract: restore binds one parameter
        // per kept manifest column and refuses a row with a missing cell, so
        // a row carrying only a subset of the declared columns refuses at
        // inspect with fixed vocabulary that never echoes row content.
        let mut subset = good_dump();
        subset[1]["data"]
            .as_object_mut()
            .unwrap()
            .shift_remove("guild_id");
        subset[1]["data"]["id"] = serde_json::json!(ECHO_SENTINEL);
        let err = acceptance_refusal(&subset);
        assert!(err.contains("is missing a manifest column"), "{err}");
        assert!(!err.contains(ECHO_SENTINEL), "{err}");
    }

    #[test]
    fn acceptance_dump_inspect_accepts_legacy_v3_without_column_types() {
        // Frozen v3 has declared columns but no column_types at all, and its
        // cells are native JSON scalars; parity checks must not refuse it.
        let mut objs = good_dump();
        objs[0]["version"] = serde_json::json!(3);
        for table in objs[0]["tables"].as_array_mut().unwrap() {
            table.as_object_mut().unwrap().shift_remove("column_types");
        }
        objs[1]["data"] = serde_json::json!({"id": 1, "guild_id": "g"});
        let contents = inspect_bytes(&gzip_lines(&objs)).unwrap();
        assert_eq!(contents.manifest.version, 3);
        assert_eq!(contents.rows, 1);
        assert!(contents
            .manifest
            .tables
            .iter()
            .all(|table| table.column_types.is_empty()));
        let events = contents
            .manifest
            .tables
            .iter()
            .find(|table| table.name == "events")
            .unwrap();
        assert_eq!(events.columns, ["id", "guild_id"]);
        assert_eq!(contents.buffers["events"][0]["id"], serde_json::json!(1));
        assert_eq!(
            contents.buffers["events"][0]["guild_id"],
            serde_json::json!("g")
        );
    }
}
