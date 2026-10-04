//! Durable cutover rollback journal and per-table watermarks (TOG-12137).
//!
//! `docs/cutover.md` precondition: a durable journal/watermark must cover
//! every affected table, deletes and the side-effect ledger — "No complete
//! rollback data path = NO-GO". The rollback window is `[T_0, T_r]` (up to
//! the full 48-hour watch), not the time since the last backup, so
//! restore-to-`T_f` alone would lose the whole window. Deletes carry no
//! `updated_at` a `WHERE updated_at > T_f` scan could find, which is why
//! every journal row carries the pre-write snapshot, not just the key.
//!
//! What this module is:
//!
//! * [`JournalEntry`]: one validated write (insert/update/delete) with the
//!   affected row's identity and its pre-image. Updates and deletes require
//!   a pre-image; without it a delete is unrestorable and an update is
//!   irreversible. Inserts carry none: rollback of an insert is the row's
//!   deletion, and there is no prior state to keep.
//! * [`record`]: appends the entry to `rollback_journal` and advances that
//!   table's watermark in the same transaction, so a committed write is
//!   always journaled and the watermark never runs ahead of the journal.
//! * [`watermark_for`] / [`restorable_point`]: the restorable cursors. The
//!   global restorable point is `MAX(id)`; per-table cursors advance with
//!   `GREATEST`, so concurrent writers keep them monotonic.
//! * [`entries_since`]: the replay read path — every journal row for a
//!   table after a baseline id, in journal order.
//! * [`COVERED_TABLES`]: the affected-table inventory the cutover procedure
//!   reconciles against (bot tables plus every cutover-chain feature table
//!   and side-effect ledger).
//!
//! What this module is not: the rollback procedure itself (that stays in the
//! runbook), and wiring into every writer (follow-up slices call [`record`]
//! from their own write paths). Pre-images are TEXT JSON blobs, matching the
//! funnel `events.metadata` convention — the journal is a restore source,
//! not a query index, so there is no JSONB and no secondary parsing.

use sqlx::{Pool, Postgres, Transaction};
use thiserror::Error;

/// Every table the cutover rollback must reconcile: the S6 bot tables plus
/// each cutover-chain feature table and side-effect ledger. The cutover
/// procedure treats a write to a table outside this list as uncovered.
pub const COVERED_TABLES: &[&str] = &[
    // S6 bot chain (`crates/store/migrations/0400-0405`).
    "events",
    "members",
    "invite_snapshots",
    "web_contract_meta",
    "guild_counters",
    "rank_ladder",
    "rank_snapshots",
    "member_ranks",
    "scheduled_events",
    "counter_snapshots",
    "member_exclusions",
    // Operational audit + delivery ledger.
    "operational_audit_log",
    "audit_kill_switch",
    "automation_audit_log",
    "announcements_audit_log",
    // Internal actions: intents, idempotency, nonces, deliveries.
    "internal_action_log",
    "internal_action_roles",
    "internal_idempotency",
    "internal_nonces",
    "internal_discord_events",
    "internal_event_keys",
    // Scheduling, gateway sessions, guild settings.
    "scheduled_messages",
    "gateway_sessions",
    "guild_settings",
    "guild_settings_revision",
    "guild_settings_audit",
    "guild_settings_versions",
    "guild_settings_allocator",
    // Engagement features.
    "sticky_messages",
    "lfg_posts",
    "lfg_roles",
    "lfg_signups",
    "event_rsvps",
    "tickets",
    "ticket_transcripts",
    "feed_relays",
    "feed_deliveries",
    "community_facts",
    "community_scorecard_runs",
    "community_scorecard_alerts",
    "community_stream_heartbeats",
    "presence_probe",
    "self_role_audit",
    "self_role_panel_claims",
    "join_risk_flags",
    // Moderation + leveling ledgers.
    "moderation_audit",
    "moderation_lockdowns",
    "moderation_channel_executions",
    "moderation_idempotency",
    "member_levels",
    "xp_cooldowns",
    "xp_awards",
    "level_role_rewards",
    "level_import_runs",
];

/// True when `table` is in the rollback inventory.
#[must_use]
pub fn is_covered(table: &str) -> bool {
    COVERED_TABLES.contains(&table)
}

/// The write operation a journal row records. Serializes to the exact
/// strings the `rollback_journal.op` CHECK constraint admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalOp {
    Insert,
    Update,
    Delete,
}

impl JournalOp {
    /// Stable lowercase name matching the SQL CHECK constraint.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }

    /// Parse back exactly what [`JournalOp::as_str`] emits; anything else
    /// (including case variants and padding) is refused.
    pub fn parse(value: &str) -> Result<Self, JournalError> {
        match value {
            "insert" => Ok(Self::Insert),
            "update" => Ok(Self::Update),
            "delete" => Ok(Self::Delete),
            _ => Err(JournalError::Invalid("unknown journal op")),
        }
    }
}

/// One write to journal. `row_identity` pins the affected row (a primary key
/// or a stable `part:part` composite from [`identity`]); `pre_image` is the
/// row's pre-write JSON snapshot, required for updates and deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub table_name: String,
    pub row_identity: String,
    pub op: JournalOp,
    pub pre_image: Option<String>,
}

impl JournalEntry {
    /// Check the entry before it touches the database: non-empty table and
    /// identity, covered table, and a pre-image wherever rollback needs one.
    pub fn validate(&self) -> Result<(), JournalError> {
        if self.table_name.trim().is_empty() || self.row_identity.trim().is_empty() {
            return Err(JournalError::Invalid("table and row identity are required"));
        }
        if !is_covered(&self.table_name) {
            return Err(JournalError::Invalid(
                "table is outside the rollback inventory",
            ));
        }
        match self.op {
            JournalOp::Insert => Ok(()),
            JournalOp::Update | JournalOp::Delete => {
                if self
                    .pre_image
                    .as_deref()
                    .is_some_and(|image| !image.trim().is_empty())
                {
                    Ok(())
                } else {
                    Err(JournalError::Invalid(
                        "updates and deletes require a pre-image",
                    ))
                }
            }
        }
    }
}

/// Join key parts into a stable composite row identity (`guild:member`).
/// Empty input yields an empty string, which [`JournalEntry::validate`]
/// refuses — identities are never silently defaulted.
#[must_use]
pub fn identity(parts: &[&str]) -> String {
    parts.join(":")
}

/// A committed journal row: the replay unit the rollback procedure reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRow {
    pub id: i64,
    pub table_name: String,
    pub row_identity: String,
    pub op: JournalOp,
    pub pre_image: Option<String>,
}

/// Journal failure: a database error, or a [`JournalEntry`] that fails
/// [`JournalEntry::validate`]. Validation runs before any SQL, so an
/// invalid entry never opens a transaction.
#[derive(Debug, Error)]
pub enum JournalError {
    #[error("rollback journal database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("invalid journal entry: {0}")]
    Invalid(&'static str),
}

/// Append `entry` and advance its table watermark atomically. The returned
/// id is the entry's journal sequence; the table watermark equals it unless
/// a concurrent writer committed a later row first, in which case the
/// watermark holds that later id and stays monotonic either way.
pub async fn record(pool: &Pool<Postgres>, entry: &JournalEntry) -> Result<i64, JournalError> {
    entry.validate()?;
    let mut tx = pool.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO rollback_journal (table_name, row_identity, op, pre_image)
         VALUES ($1, $2, $3, $4)
         RETURNING id",
    )
    .bind(&entry.table_name)
    .bind(&entry.row_identity)
    .bind(entry.op.as_str())
    .bind(entry.pre_image.as_deref())
    .fetch_one(&mut *tx)
    .await?;
    advance_in(&mut tx, &entry.table_name, id).await?;
    tx.commit().await?;
    Ok(id)
}

/// Advance `table`'s watermark to at least `journal_id`. `GREATEST` keeps the
/// cursor monotonic under concurrent writers: a stale advance is a no-op,
/// never a rewind.
pub async fn advance_watermark(
    pool: &Pool<Postgres>,
    table: &str,
    journal_id: i64,
) -> Result<(), JournalError> {
    if !is_covered(table) {
        return Err(JournalError::Invalid(
            "table is outside the rollback inventory",
        ));
    }
    let mut tx = pool.begin().await?;
    advance_in(&mut tx, table, journal_id).await?;
    tx.commit().await?;
    Ok(())
}

async fn advance_in(
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
    journal_id: i64,
) -> Result<(), JournalError> {
    sqlx::query(
        "INSERT INTO rollback_watermarks (table_name, last_journal_id, updated_at)
         VALUES ($1, $2, date_trunc('milliseconds', now()))
         ON CONFLICT (table_name) DO UPDATE SET
           last_journal_id = GREATEST(rollback_watermarks.last_journal_id, EXCLUDED.last_journal_id),
           updated_at = date_trunc('milliseconds', now())",
    )
    .bind(table)
    .bind(journal_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The durable cursor for `table`: the newest journal id its writers have
/// committed. `None` means no writer has journaled that table yet — not
/// that the table is empty, so the procedure must reconcile from the
/// baseline, never assume zero writes.
pub async fn watermark_for(
    pool: &Pool<Postgres>,
    table: &str,
) -> Result<Option<i64>, JournalError> {
    let id: Option<i64> =
        sqlx::query_scalar("SELECT last_journal_id FROM rollback_watermarks WHERE table_name = $1")
            .bind(table)
            .fetch_optional(pool)
            .await?;
    Ok(id)
}

/// The global restorable point: the newest journal id across all tables.
/// Rollback replays every row after the pre-window baseline up to this id.
/// Zero on an empty journal. `BIGSERIAL` ids commit out of order under
/// concurrent writers, so a point of N does not prove every id below N has
/// committed: read it, and replay, only after the bot's writers have stopped.
pub async fn restorable_point(pool: &Pool<Postgres>) -> Result<i64, JournalError> {
    let id: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM rollback_journal")
        .fetch_one(pool)
        .await?;
    Ok(id)
}

/// Every journal row for `table` after `since_id`, in journal order. This is
/// the replay input: applying these rows (inserts re-applied, updates and
/// deletes restored from pre-images) reconciles the table to the watermark.
pub async fn entries_since(
    pool: &Pool<Postgres>,
    table: &str,
    since_id: i64,
) -> Result<Vec<JournalRow>, JournalError> {
    let rows: Vec<(i64, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, table_name, row_identity, op, pre_image
         FROM rollback_journal
         WHERE table_name = $1 AND id > $2
         ORDER BY id",
    )
    .bind(table)
    .bind(since_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(id, table_name, row_identity, op, pre_image)| {
            Ok(JournalRow {
                id,
                table_name,
                row_identity,
                op: JournalOp::parse(&op)?,
                pre_image,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure merge the watermark upsert applies: the cursor never rewinds.
    fn merge_watermark(current: i64, candidate: i64) -> i64 {
        current.max(candidate)
    }

    #[test]
    fn op_names_round_trip_and_refuse_garbage() {
        for op in [JournalOp::Insert, JournalOp::Update, JournalOp::Delete] {
            assert_eq!(JournalOp::parse(op.as_str()).unwrap(), op);
        }
        for garbage in [
            "", "INSERT", "Delete", " insert", "insert ", "upsert", "del", "create",
        ] {
            assert!(
                JournalOp::parse(garbage).is_err(),
                "op must refuse {garbage:?}"
            );
        }
    }

    #[test]
    fn validation_requires_identity_inventory_and_pre_images() {
        let base = JournalEntry {
            table_name: "members".to_owned(),
            row_identity: identity(&["42", "123"]),
            op: JournalOp::Update,
            pre_image: Some(r#"{"left_at":null}"#.to_owned()),
        };
        assert!(base.validate().is_ok());

        let insert = JournalEntry {
            op: JournalOp::Insert,
            pre_image: None,
            ..base.clone()
        };
        assert!(insert.validate().is_ok());

        let no_image = JournalEntry {
            pre_image: None,
            ..base.clone()
        };
        assert!(no_image.validate().is_err());
        let blank_image = JournalEntry {
            pre_image: Some("  ".to_owned()),
            op: JournalOp::Delete,
            ..base.clone()
        };
        assert!(blank_image.validate().is_err());

        for bad in [
            JournalEntry {
                table_name: String::new(),
                ..base.clone()
            },
            JournalEntry {
                table_name: "   ".to_owned(),
                ..base.clone()
            },
            JournalEntry {
                row_identity: String::new(),
                ..base.clone()
            },
            JournalEntry {
                table_name: "pg_stat_activity".to_owned(),
                ..base.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "entry must refuse {bad:?}");
        }
    }

    #[test]
    fn identity_joins_parts_and_never_defaults() {
        assert_eq!(identity(&["42", "123"]), "42:123");
        assert_eq!(identity(&["single"]), "single");
        assert_eq!(identity(&[]), "");
        // Joining is structural: splitting the composite recovers the parts.
        let parts = ["guild-9", "member-7", "extra"];
        let joined = identity(&parts);
        assert_eq!(joined.split(':').collect::<Vec<_>>(), parts);
    }

    #[test]
    fn watermark_merge_is_monotonic_over_generated_pairs() {
        // Property: merge never returns less than either input, and equals
        // the greater one — the GREATEST the upsert applies in SQL.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..256 {
            let current = (next() % 1_000_000) as i64;
            let candidate = (next() % 1_000_000) as i64;
            let merged = merge_watermark(current, candidate);
            assert_eq!(merged, current.max(candidate));
            assert!(merged >= current && merged >= candidate);
        }
        // Stale advances are no-ops, including at the zero boundary.
        assert_eq!(merge_watermark(9, 4), 9);
        assert_eq!(merge_watermark(0, 0), 0);
    }

    #[test]
    fn pre_images_survive_a_json_round_trip() {
        // Pre-images are TEXT blobs written with insertion order preserved;
        // the journal stores them verbatim, so a serde round trip here pins
        // the shape writers must produce.
        let image = serde_json::json!({
            "guild_id": "42",
            "member_id": "123",
            "left_at": serde_json::Value::Null,
            "is_bot": false,
        });
        let text = image.to_string();
        let entry = JournalEntry {
            table_name: "members".to_owned(),
            row_identity: identity(&["42", "123"]),
            op: JournalOp::Delete,
            pre_image: Some(text.clone()),
        };
        assert!(entry.validate().is_ok());
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, image);
    }

    #[test]
    fn covered_inventory_has_no_duplicates_or_blanks() {
        let mut seen = std::collections::HashSet::new();
        for table in COVERED_TABLES {
            assert!(!table.trim().is_empty(), "inventory has a blank table");
            assert_eq!(*table, table.trim(), "inventory table is not trimmed");
            assert!(seen.insert(table), "inventory lists {table} twice");
        }
        // The funnel core and the side-effect ledgers anchor the inventory.
        for anchor in [
            "events",
            "members",
            "operational_audit_log",
            "internal_action_log",
            "scheduled_messages",
            "gateway_sessions",
        ] {
            assert!(is_covered(anchor), "inventory must cover {anchor}");
        }
        assert!(!is_covered("pg_stat_activity"));
    }
}
