//! Deterministic raw-expiry deadline and bounded-batch contract.
//!
//! The existing `run_retention(pool, mode)` entry point has no injectable clock
//! or row budget. This separate target deliberately specifies the missing seam
//! without making the baseline behavioral target depend on an invented symbol:
//!
//! ```text
//! pub async fn run_raw_expiry_batch(
//!     pool: &PgPool,
//!     mode: RetentionMode,
//!     at: OffsetDateTime,
//!     next_sweep_deadline: OffsetDateTime,
//!     max_rows: u32,
//! ) -> Result<RetentionReport, sqlx::Error>;
//! ```
//!
//! This seam belongs to the existing retention executor, NOT another scheduler.
//! The lifecycle owner must supply a bounded next-successful-sweep deadline and
//! drain nonempty batches in that cycle. The tested 60-second horizon is an
//! input, not a claimed deployment setting. Waiting a full cadence after a row
//! reaches 30/45 days violates the maximum: reserve that cadence as headroom.
//! Therefore event_time <= next_sweep_deadline - retention_window is eligible,
//! equality is inclusive, and comparisons are instants in UTC, not date strings.
//! A timestamp one microsecond newer is outside this sweep's eligibility.
//!
//! The cap is GLOBAL across presence, voice and audit classes. Dry-run reports
//! the same bounded candidate set and commits nothing. Invalid/zero budgets or
//! nonpositive horizons fail without writes. Backlogs need repeated bounded
//! calls within the same lifecycle cycle; one bounded DELETE alone is not a
//! maximum-age guarantee. Runtime backlog/overdue alerting and actual cadence
//! remain subject to the existing lifecycle acceptance and later QA evidence.
//!
//! On the pinned baseline this seam does not exist. A compiler error for this
//! target is NOT an observed assertion RED; record it separately from the
//! baseline behavior target and from any cache/environment refusal.

#[path = "support/raw_expiry.rs"]
mod support;

use support::{insert_capture, snapshot, with_database, GUILD_A, GUILD_B};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};
use twilight_model::gateway::event::EventType as Dispatch;
use two_bot_cutover::retention::{run_raw_expiry_batch, RetentionMode};

fn instant(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).expect("fixed RFC3339 fixture instant")
}

#[tokio::test]
async fn inclusive_source_cutoffs_are_utc_instants_with_cadence_headroom() {
    with_database(|pool| async move {
        let at = instant("2026-10-13T12:00:00Z");
        let deadline = instant("2026-10-13T12:01:00Z");
        for (kind, guild, key, source) in [
            (
                Dispatch::PresenceUpdate,
                GUILD_A,
                "presence-before",
                "2026-09-13T12:00:59Z",
            ),
            (
                Dispatch::PresenceUpdate,
                GUILD_A,
                "presence-equal",
                "2026-09-13T12:01:00Z",
            ),
            (
                Dispatch::PresenceUpdate,
                GUILD_A,
                "presence-after",
                "2026-09-13T12:01:00.000001Z",
            ),
            (
                Dispatch::PresenceUpdate,
                GUILD_B,
                "presence-offset-equal",
                "2026-09-13T08:01:00-04:00",
            ),
            (
                Dispatch::VoiceStateUpdate,
                GUILD_A,
                "voice-equal",
                "2026-09-13T12:01:00Z",
            ),
            (
                Dispatch::VoiceServerUpdate,
                GUILD_B,
                "voice-after",
                "2026-09-13T12:01:00.000001Z",
            ),
            (
                Dispatch::GuildAuditLogEntryCreate,
                GUILD_A,
                "audit-equal",
                "2026-08-29T08:01:00-04:00",
            ),
            (
                Dispatch::GuildAuditLogEntryCreate,
                GUILD_B,
                "audit-after",
                "2026-08-29T12:01:00.000001Z",
            ),
        ] {
            insert_capture(&pool, kind, guild, key, instant(source), at).await;
        }
        let before = snapshot(&pool).await;
        let dry = run_raw_expiry_batch(&pool, RetentionMode::DryRun, at, deadline, 20)
            .await
            .expect("plan deterministic raw batch");
        assert_eq!(snapshot(&pool).await, before);
        assert_eq!(
            (
                dry.presence_raw_deleted,
                dry.voice_raw_deleted,
                dry.audit_cache_deleted
            ),
            (3, 1, 1)
        );
        let live = run_raw_expiry_batch(&pool, RetentionMode::Execute, at, deadline, 20)
            .await
            .expect("execute deterministic raw batch");
        assert_eq!(
            (
                live.presence_raw_deleted,
                live.voice_raw_deleted,
                live.audit_cache_deleted
            ),
            (3, 1, 1)
        );
        let survivors: Vec<String> =
            sqlx::query_scalar("SELECT dedup_key FROM event_store ORDER BY dedup_key")
                .fetch_all(&pool)
                .await
                .expect("read boundary survivors");
        assert_eq!(survivors, ["audit-after", "presence-after", "voice-after"]);
    })
    .await;
}

#[tokio::test]
async fn global_row_budget_drains_backlog_in_bounded_catch_up_batches() {
    with_database(|pool| async move {
        let at = instant("2026-10-13T12:00:00Z");
        let deadline = at + Duration::seconds(60);
        for (kind, guild, key) in [
            (Dispatch::PresenceUpdate, GUILD_A, "batch-p1"),
            (Dispatch::PresenceUpdate, GUILD_B, "batch-p2"),
            (Dispatch::PresenceUpdate, GUILD_A, "batch-p3"),
            (Dispatch::PresenceUpdate, GUILD_B, "batch-p4"),
            (Dispatch::VoiceStateUpdate, GUILD_A, "batch-v1"),
            (Dispatch::VoiceServerUpdate, GUILD_B, "batch-v2"),
            (Dispatch::GuildAuditLogEntryCreate, GUILD_A, "batch-a1"),
        ] {
            insert_capture(&pool, kind, guild, key, at - Duration::days(90), at).await;
        }
        insert_capture(
            &pool,
            Dispatch::MessageCreate,
            GUILD_B,
            "keep-unrelated",
            at - Duration::days(90),
            at,
        )
        .await;
        insert_capture(
            &pool,
            Dispatch::PresenceUpdate,
            GUILD_A,
            "keep-fresh",
            at,
            at,
        )
        .await;
        let before = snapshot(&pool).await;
        let dry = run_raw_expiry_batch(&pool, RetentionMode::DryRun, at, deadline, 2)
            .await
            .expect("plan bounded catch-up");
        assert_eq!(
            dry.total_deleted(),
            2,
            "the cap is not per class or per guild"
        );
        assert_eq!(snapshot(&pool).await, before);
        let mut class_totals = (0, 0, 0);
        let mut remaining = 9_i64;
        for expected_deleted in [2_u64, 2, 2, 1] {
            let report = run_raw_expiry_batch(&pool, RetentionMode::Execute, at, deadline, 2)
                .await
                .expect("execute bounded catch-up");
            assert_eq!(report.total_deleted(), expected_deleted);
            class_totals.0 += report.presence_raw_deleted;
            class_totals.1 += report.voice_raw_deleted;
            class_totals.2 += report.audit_cache_deleted;
            remaining -= i64::try_from(expected_deleted).expect("small fixture count");
            let actual: i64 = sqlx::query_scalar("SELECT count(*) FROM event_store")
                .fetch_one(&pool)
                .await
                .expect("read actual batch effects");
            assert_eq!(
                actual, remaining,
                "reported counts must equal committed effects"
            );
        }
        assert_eq!(class_totals, (4, 2, 1));
        let empty = run_raw_expiry_batch(&pool, RetentionMode::Execute, at, deadline, 2)
            .await
            .expect("verify backlog drained");
        assert_eq!(empty.total_deleted(), 0);
        let survivors: Vec<String> =
            sqlx::query_scalar("SELECT dedup_key FROM event_store ORDER BY dedup_key")
                .fetch_all(&pool)
                .await
                .expect("read bounded survivors");
        assert_eq!(survivors, ["keep-fresh", "keep-unrelated"]);
    })
    .await;
}

#[tokio::test]
async fn zero_budget_or_nonpositive_horizon_is_refused_without_writes() {
    with_database(|pool| async move {
        let at = instant("2026-10-13T12:00:00Z");
        insert_capture(
            &pool,
            Dispatch::PresenceUpdate,
            GUILD_A,
            "invalid-policy",
            at - Duration::days(31),
            at,
        )
        .await;
        let before = snapshot(&pool).await;
        for (deadline, budget) in [
            (at + Duration::seconds(60), 0),
            (at, 2),
            (at - Duration::seconds(1), 2),
        ] {
            assert!(
                run_raw_expiry_batch(&pool, RetentionMode::Execute, at, deadline, budget)
                    .await
                    .is_err(),
                "invalid expiry budget/horizon must fail closed"
            );
            assert_eq!(snapshot(&pool).await, before);
        }
    })
    .await;
}
