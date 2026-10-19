//! Raw-expiry acceptance, independent of the earlier retention suites.
//!
//! Funnel fixtures use the producer's typed values, not the historical
//! `voice_session_started` / `voice_session_ended` spellings in `events`.
//! Raw fixtures use Twilight's actual dispatch names and the merged capture
//! schema. Main has no public raw-capture writer; these tests do not claim
//! producer wiring or installed ingest-role privileges have been verified.
//!
//! `event_time` is the persisted event/observation timestamp; `received_at`
//! records insertion. Presence has no Discord source timestamp: its original
//! observation time must not be replaced by a retry/backfill insertion time.
//! A successful manual sweep is not proof of a live maximum-age guarantee.
//! Deterministic deadline/budget cases live in `retention_raw_expiry_bounds`;
//! startup, cadence and cancellation must reuse the existing runtime lifecycle.

#[path = "support/raw_expiry.rs"]
mod support;

use std::time::Instant;

use sqlx::PgPool;
use support::{insert_capture, snapshot, with_database, GUILD_A, GUILD_B, MEMBER};
use time::{Duration, OffsetDateTime};
use twilight_model::gateway::event::EventType as Dispatch;
use two_bot_core::funnel::EventType;
use two_bot_core::member_moderation::{ClaimState, MemberModerationStore};
use two_bot_core::member_moderation_store::PgMemberModerationStore;
use two_bot_cutover::retention::{
    handle_guild_delete, request_user_deletion, run_retention, RetentionMode,
};

async fn now(pool: &PgPool) -> OffsetDateTime {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await
        .expect("read fixture clock")
}

async fn seed_funnel_voice(pool: &PgPool) {
    for (guild, prefix) in [(GUILD_A, "a"), (GUILD_B, "b")] {
        for kind in [EventType::VoiceSessionStart, EventType::VoiceSessionEnd] {
            for (age_days, suffix) in [(31, "old"), (1, "fresh")] {
                sqlx::query(
                    "INSERT INTO events
                     (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
                     VALUES ($1, $2, $3, now() - make_interval(days => $4), 'gateway', '{}', $5)",
                )
                .bind(kind.as_str())
                .bind(MEMBER)
                .bind(guild)
                .bind(age_days)
                .bind(format!("raw-{prefix}-{}-{suffix}", kind.as_str()))
                .execute(pool)
                .await
                .expect("seed real funnel event values");
            }
        }
    }
}

async fn seed_capture_windows(pool: &PgPool) {
    let received = now(pool).await;
    for (guild, prefix) in [(GUILD_A, "a"), (GUILD_B, "b")] {
        for (kind, label, old_days) in [
            (Dispatch::PresenceUpdate, "presence", 31),
            (Dispatch::VoiceStateUpdate, "voice-state", 31),
            (Dispatch::VoiceServerUpdate, "voice-server", 31),
            (Dispatch::GuildAuditLogEntryCreate, "audit", 46),
        ] {
            for (days, suffix) in [(old_days, "old"), (1, "fresh")] {
                assert_eq!(
                    insert_capture(
                        pool,
                        kind,
                        guild,
                        &format!("{prefix}-{label}-{suffix}"),
                        received - Duration::days(days),
                        received,
                    )
                    .await,
                    1
                );
            }
        }
        insert_capture(
            pool,
            Dispatch::MessageCreate,
            guild,
            &format!("{prefix}-unrelated"),
            received - Duration::days(90),
            received,
        )
        .await;
    }
}

async fn capture_rows(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT dedup_key, to_jsonb(e)::text FROM event_store e ORDER BY dedup_key")
        .fetch_all(pool)
        .await
        .expect("read complete capture rows")
}

async fn seed_protected(pool: &PgPool) {
    sqlx::raw_sql(include_str!("fixtures/raw_expiry_protected.sql"))
        .execute(pool)
        .await
        .expect("seed retained aggregate, audit and replay state");
}

async fn protected_rows(pool: &PgPool) -> Vec<(String, String)> {
    // Expiry may append its own run telemetry; it may not mutate these seeded
    // aggregates, unrelated events, control records, queues or deletion receipts.
    let retained = [
        "member_levels",
        "member_ranks",
        "events",
        "guild_settings_audit",
        "privacy_opt_outs",
        "privacy_opt_out_audit",
        "guild_analytics_settings",
        "moderation_idempotency",
        "moderation_channel_executions",
        "internal_idempotency",
        "retention_guild_purge_queue",
        "retention_deletion_requests",
    ];
    snapshot(pool)
        .await
        .into_iter()
        .filter(|(table, _)| retained.contains(&table.as_str()))
        .collect()
}

#[tokio::test]
async fn funnel_voice_names_expire_without_touching_fresh_rows() {
    with_database(|pool| async move {
        seed_funnel_voice(&pool).await;
        let report = run_retention(&pool, RetentionMode::Execute)
            .await
            .expect("execute retention");
        let survivors: Vec<String> =
            sqlx::query_scalar("SELECT idempotency_key FROM events ORDER BY idempotency_key")
                .fetch_all(&pool)
                .await
                .expect("read funnel survivors");
        assert_eq!(
            report.voice_raw_deleted, 4,
            "real VoiceSessionStart/End rows from both guilds must expire"
        );
        assert_eq!(
            survivors,
            [
                "raw-a-voice_session_end-fresh",
                "raw-a-voice_session_start-fresh",
                "raw-b-voice_session_end-fresh",
                "raw-b-voice_session_start-fresh",
            ]
        );
        let repeated = run_retention(&pool, RetentionMode::Execute)
            .await
            .expect("repeat retention");
        assert_eq!(repeated.total_deleted(), 0);
    })
    .await;
}

#[tokio::test]
async fn capture_expiry_uses_event_time_despite_recent_receipt_in_both_guilds() {
    with_database(|pool| async move {
        seed_capture_windows(&pool).await;
        let before = capture_rows(&pool).await;
        let keep = [
            "a-audit-fresh",
            "a-presence-fresh",
            "a-unrelated",
            "a-voice-server-fresh",
            "a-voice-state-fresh",
            "b-audit-fresh",
            "b-presence-fresh",
            "b-unrelated",
            "b-voice-server-fresh",
            "b-voice-state-fresh",
        ];
        let expected: Vec<_> = before
            .into_iter()
            .filter(|(key, _)| keep.contains(&key.as_str()))
            .collect();
        assert_eq!(expected.len(), 10, "fixture must include every survivor");
        let report = run_retention(&pool, RetentionMode::Execute)
            .await
            .expect("execute raw capture expiry");
        assert_eq!(
            (
                report.presence_raw_deleted,
                report.voice_raw_deleted,
                report.audit_cache_deleted
            ),
            (2, 4, 2),
            "count actual expired rows, including anonymous voice-server rows"
        );
        assert_eq!(
            capture_rows(&pool).await,
            expected,
            "survivors must be byte-identical"
        );
        let again = run_retention(&pool, RetentionMode::Execute)
            .await
            .expect("repeat raw capture expiry");
        assert_eq!(again.total_deleted(), 0);
        assert_eq!(capture_rows(&pool).await, expected);
    })
    .await;
}

#[tokio::test]
async fn dry_run_reports_each_raw_class_without_mutating_data_queues_or_receipts() {
    with_database(|pool| async move {
        seed_funnel_voice(&pool).await;
        seed_capture_windows(&pool).await;
        seed_protected(&pool).await;
        const QUEUED_GUILD: &str = "666666666666666666";
        sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2)")
            .bind(QUEUED_GUILD)
            .bind(MEMBER)
            .execute(&pool)
            .await
            .expect("seed queued member");
        handle_guild_delete(&pool, QUEUED_GUILD, None)
            .await
            .expect("enqueue fixture guild purge");
        request_user_deletion(&pool, QUEUED_GUILD, MEMBER)
            .await
            .expect("enqueue fixture deletion");
        let before = snapshot(&pool).await;
        let dry = run_retention(&pool, RetentionMode::DryRun)
            .await
            .expect("dry-run retention");
        assert_eq!(
            snapshot(&pool).await,
            before,
            "dry-run changed rows or sequences"
        );
        assert_eq!(
            (
                dry.presence_raw_deleted,
                dry.voice_raw_deleted,
                dry.audit_cache_deleted
            ),
            (2, 8, 2),
            "would-delete counts include both raw capture and real funnel voice names"
        );
        let repeated = run_retention(&pool, RetentionMode::DryRun)
            .await
            .expect("repeat dry-run");
        assert_eq!(repeated.total_deleted(), dry.total_deleted());
        assert_eq!(snapshot(&pool).await, before);
    })
    .await;
}

#[tokio::test]
async fn raw_expiry_preserves_aggregates_audits_receipts_and_effect_suppression_fences() {
    with_database(|pool| async move {
        seed_capture_windows(&pool).await;
        seed_protected(&pool).await;
        let protected_before = protected_rows(&pool).await;
        let report = run_retention(&pool, RetentionMode::Execute)
            .await
            .expect("expire raw rows beside retained state");
        let protected_after = protected_rows(&pool).await;
        assert_eq!(
            protected_after, protected_before,
            "age expiry changed retained state"
        );
        assert_eq!(
            report.total_deleted(),
            8,
            "preservation must not be a no-op expiry"
        );

        let ledger = PgMemberModerationStore::new(pool.clone(), GUILD_A);
        let replay = ledger
            .claim(
                GUILD_A,
                "raw-retained-release",
                "warn",
                "operator_released",
                "2026-10-13T12:00:00Z",
            )
            .await
            .expect("read retained replay decision");
        assert!(
            matches!(replay, ClaimState::Replayed { outcome } if outcome == "operator_released")
        );
        let original = ledger
            .claim(
                GUILD_A,
                "raw-retained-release",
                "warn",
                "original-request",
                "2026-10-13T12:00:00Z",
            )
            .await
            .expect("refuse old effect after expiry");
        assert!(
            matches!(original, ClaimState::Mismatch),
            "old Discord effect became claimable"
        );
        for family in [
            "voice_session_start",
            "PRESENCE_UPDATE",
            "VOICE_STATE_UPDATE",
        ] {
            assert!(
                two_bot_core::privacy::ingestion_blocked(&pool, GUILD_A, Some(MEMBER), family)
                    .await
                    .expect("check retained opt-out")
            );
            assert!(!two_bot_core::privacy::ingestion_blocked(
                &pool,
                GUILD_B,
                Some(MEMBER),
                family
            )
            .await
            .expect("check cross-guild isolation"));
        }
    })
    .await;
}

#[tokio::test]
async fn real_append_only_audit_guard_remains_active_after_expiry() {
    with_database(|pool| async move {
        seed_capture_windows(&pool).await;
        seed_protected(&pool).await;
        for after_expiry in [false, true] {
            if after_expiry {
                run_retention(&pool, RetentionMode::Execute)
                    .await
                    .expect("expiry must not attempt immutable audit mutation");
            }
            for statement in [
                "DELETE FROM guild_settings_audit WHERE false",
                "UPDATE guild_settings_audit SET actor = actor WHERE false",
            ] {
                let error = sqlx::query(statement).execute(&pool).await.expect_err(
                    "the real statement-level append-only trigger must reject even zero rows",
                );
                assert_eq!(
                    error
                        .as_database_error()
                        .and_then(|error| error.code())
                        .as_deref(),
                    Some("P0001"),
                    "must be the append-only trigger, not an unrelated fixture/setup error"
                );
            }
        }
        let retained: i64 = sqlx::query_scalar("SELECT count(*) FROM guild_settings_audit")
            .fetch_one(&pool)
            .await
            .expect("read immutable audit count");
        assert_eq!(retained, 1);
    })
    .await;
}

#[tokio::test]
async fn raw_failure_rolls_back_without_false_success_and_retry_is_idempotent() {
    with_database(|pool| async move {
        let received = now(&pool).await;
        insert_capture(&pool, Dispatch::PresenceUpdate, GUILD_A, "retry-presence",
            received - Duration::days(31), received).await;
        insert_capture(&pool, Dispatch::VoiceStateUpdate, GUILD_B, "retry-voice",
            received - Duration::days(31), received).await;
        seed_protected(&pool).await;
        // An additive test-only FK supplies a real database failure. No trigger
        // is disabled, no privileges change, and no production guard is bypassed.
        sqlx::raw_sql(
            "CREATE TABLE raw_expiry_wedge (id BIGINT PRIMARY KEY REFERENCES event_store(id));
             INSERT INTO raw_expiry_wedge SELECT id FROM event_store WHERE dedup_key = 'retry-voice';",
        ).execute(&pool).await.expect("install disposable failure fixture");
        let before = capture_rows(&pool).await;
        let retained = protected_rows(&pool).await;
        let refused = run_retention(&pool, RetentionMode::Execute).await;
        assert!(refused.is_err(), "raw deletion failure must not report success");
        assert_eq!(capture_rows(&pool).await, before, "failed raw batch partially committed");
        assert_eq!(protected_rows(&pool).await, retained, "failure altered fences or receipts");
        sqlx::query("DELETE FROM raw_expiry_wedge").execute(&pool).await
            .expect("remove only the disposable failure row");
        let retried = run_retention(&pool, RetentionMode::Execute).await.expect("retry raw expiry");
        assert_eq!((retried.presence_raw_deleted, retried.voice_raw_deleted), (1, 1));
        let repeated = run_retention(&pool, RetentionMode::Execute).await.expect("repeat successful expiry");
        assert_eq!(repeated.total_deleted(), 0);
    })
    .await;
}

#[tokio::test]
async fn reported_duration_includes_real_database_expiry_work() {
    with_database(|pool| async move {
        let received = now(&pool).await;
        insert_capture(
            &pool,
            Dispatch::PresenceUpdate,
            GUILD_A,
            "timed-presence",
            received - Duration::days(31),
            received,
        )
        .await;
        sqlx::raw_sql(
            "CREATE FUNCTION raw_expiry_delay() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN PERFORM pg_sleep(0.025); RETURN OLD; END $$;
             CREATE TRIGGER raw_expiry_delay BEFORE DELETE ON event_store
             FOR EACH ROW EXECUTE FUNCTION raw_expiry_delay();",
        )
        .execute(&pool)
        .await
        .expect("install disposable timing fixture");
        let started = Instant::now();
        let report = run_retention(&pool, RetentionMode::Execute)
            .await
            .expect("timed expiry");
        let elapsed = started.elapsed().as_millis();
        assert_eq!(report.presence_raw_deleted, 1);
        assert!(
            report.duration_ms >= 20,
            "duration omitted actual database work"
        );
        assert!(
            u128::from(report.duration_ms) <= elapsed + 1,
            "duration exceeds caller wall time"
        );
    })
    .await;
}
