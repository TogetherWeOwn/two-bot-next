#![cfg(feature = "db")]

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use two_bot_core::member_moderation::{
    AuditRow, ClaimState, DiscordError, MemberExecution, MemberModerationService,
    MemberModerationStore, MockMemberDiscord,
};
use two_bot_core::member_moderation_store::PgMemberModerationStore;
use two_bot_core::{ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget};

#[path = "support/member_moderation_backup.rs"]
mod backup;

#[path = "support/member_moderation_retry.rs"]
mod retry;

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_repeated_refusal_yields_across_consumer_reconstruction() {
    let (admin, pool, schema) = database().await;
    retry::repeated_refusal_yields(
        || PgMemberModerationStore::new(pool.clone(), "100000000000000001"),
        policy(),
    )
    .await;
    let row = sqlx::query("SELECT retry_generation, execute_at = $1::text::timestamptz AS original_expiry, claim_token IS NULL AND NOT dispatch_uncertain AS safe_pending FROM moderation_scheduled_unbans WHERE request_id = 'refused'")
        .bind(NOW).fetch_one(&pool).await.unwrap();
    assert!(row.get::<i64, _>("retry_generation") > generation(&pool, "newer").await);
    assert!(row.get::<bool, _>("original_expiry"));
    assert!(row.get::<bool, _>("safe_pending"));
    assert_eq!(unban_state(&pool, "refused").await, "pending");
    assert_eq!(unban_state(&pool, "later").await, "done");
    assert_eq!(unban_state(&pool, "newer").await, "done");
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_unknown_unban_outcome_never_receives_a_retry_position() {
    let (admin, pool, schema) = database().await;
    retry::unknown_outcome_stays_fenced(
        || PgMemberModerationStore::new(pool.clone(), "100000000000000001"),
        policy(),
    )
    .await;
    let row = sqlx::query("SELECT retry_generation IS NULL AS no_retry, claim_token IS NOT NULL AND dispatch_uncertain AS fenced FROM moderation_scheduled_unbans WHERE request_id = 'refused'")
        .fetch_one(&pool).await.unwrap();
    assert!(row.get::<bool, _>("no_retry"));
    assert!(row.get::<bool, _>("fenced"));
    assert_eq!(unban_state(&pool, "refused").await, "running");
    cleanup(admin, pool, schema).await;
}

const NOW: &str = "2023-11-14T22:13:20.000Z";
const DUE: &str = "2023-11-14T23:13:20.000Z";

// No DATABASE_URL or inherited credentials: tests accept only the approved
// agent container or the ephemeral Postgres service in GitHub Actions.
async fn database_schema() -> (PgPool, PgPool, String) {
    let host = match std::env::var("MEMBER_TESTDB").as_deref() {
        Ok("agent-testdb") => "agent-testdb",
        Ok("ci") if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") => "localhost",
        _ => panic!("set MEMBER_TESTDB=agent-testdb (or ci inside GitHub Actions)"),
    };
    let options = PgConnectOptions::new()
        .host(host)
        .port(5432)
        .username("agent_test")
        .password("")
        .database("postgres");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options.clone())
        .await
        .expect("approved test DB must connect; no credential fallback");
    let schema = format!("member_moderation_test_{:032x}", rand::random::<u128>());
    // Identifier is a fixed prefix + generated hexadecimal, never user input.
    QueryBuilder::<Postgres>::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&admin)
        .await
        .expect("test schema");
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |connection, _| {
            let query = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(query)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .expect("schema pool");
    (admin, pool, schema)
}

async fn database() -> (PgPool, PgPool, String) {
    let (admin, pool, schema) = database_schema().await;
    for _ in 0..2 {
        for migration in [
            include_str!("../../cutover/migrations/0110_moderation_member.sql"),
            include_str!("../../cutover/migrations/0111_moderation_ban_ownership.sql"),
            include_str!("../../cutover/migrations/0112_moderation_legacy_timestamps.sql"),
            include_str!("../../cutover/migrations/0113_moderation_unban_retry_order.sql"),
        ] {
            sqlx::raw_sql(migration)
                .execute(&pool)
                .await
                .expect("idempotent member migrations");
        }
    }
    (admin, pool, schema)
}

async fn cleanup(admin: PgPool, pool: PgPool, schema: String) {
    pool.close().await;
    // Only this test's generated scratch schema is removed.
    QueryBuilder::<Postgres>::new("DROP SCHEMA ")
        .push(&schema)
        .push(" CASCADE")
        .build()
        .execute(&admin)
        .await
        .expect("test cleanup");
    admin.close().await;
}

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: "123456789012345678".into(),
        bot_user_id: Some("555555555555555555".into()),
        protected_role_ids: HashSet::new(),
    }
}

async fn accepted_unban(
    store: &PgMemberModerationStore,
    guild: &str,
    user: &str,
    request: &str,
    execute_at: &str,
    created_at: &str,
) {
    let attempt = store
        .stage_unban(guild, user, execute_at, "expiry", request, created_at)
        .await
        .expect("prepared expiry");
    store
        .confirm_ban_attempt(guild, user, request, attempt, created_at)
        .await
        .expect("accepted ban");
}

async fn ban_state(pool: &PgPool, request: &str) -> String {
    sqlx::query_scalar("SELECT state FROM moderation_member_bans WHERE request_id = $1")
        .bind(request)
        .fetch_one(pool)
        .await
        .expect("ban intent state")
}

async fn unban_state(pool: &PgPool, request: &str) -> String {
    sqlx::query_scalar("SELECT state FROM moderation_scheduled_unbans WHERE request_id = $1")
        .bind(request)
        .fetch_one(pool)
        .await
        .expect("schedule state")
}

// Explicit identities of imported fixture operations. Real outcome evidence
// must retain the identity from staging, not look up the current retry here.
async fn fixture_attempt(
    pool: &PgPool,
    request: &str,
) -> two_bot_core::member_moderation::BanAttempt {
    two_bot_core::member_moderation::BanAttempt {
        generation: generation(pool, request).await,
    }
}

async fn generation(pool: &PgPool, request: &str) -> i64 {
    sqlx::query_scalar("SELECT generation FROM moderation_member_bans WHERE request_id = $1")
        .bind(request)
        .fetch_one(pool)
        .await
        .expect("persisted generation")
}

async fn put_snapshot(pool: &PgPool, request: &str) -> String {
    sqlx::query_scalar(
        "SELECT json_build_object('intent', row_to_json(intent),
           'expiry', (SELECT row_to_json(job) FROM moderation_scheduled_unbans AS job
                      WHERE job.request_id = intent.request_id))::text
         FROM moderation_member_bans AS intent WHERE intent.request_id = $1",
    )
    .bind(request)
    .fetch_one(pool)
    .await
    .expect("complete attempt and expiry snapshot")
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_historical_acceptance_requires_exact_ordering_and_atomic_audit() {
    use two_bot_core::member_moderation::{BanAttempt, HistoricalBanAcceptance};
    let (admin, pool, schema) = database().await;
    let guild = "100000000000000001";
    let store = PgMemberModerationStore::new(pool.clone(), guild);
    for (temporary, user) in [(false, "333333333333333333"), (true, "444444444444444444")] {
        let older_id = format!("historical-older-{user}");
        let later_id = format!("historical-later-{user}");
        let discord = MockMemberDiscord::new();
        let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
            1_700_003_600_000
        });
        let mut exec = execution(ModerationAction::TempBan, &older_id);
        exec.target.as_mut().unwrap().user_id = user.into();
        discord.fail_with("ban", DiscordError::Timeout);
        assert!(svc.execute(&exec).await.is_err());
        let older = fixture_attempt(&pool, &older_id).await;
        // Explicit import of a later accepted remote PUT, not normal staging
        // through a prepared fence; ordering proof comes separately below.
        let later = BanAttempt { generation: sqlx::query_scalar(
            "INSERT INTO moderation_member_bans (request_id, guild_id, user_id, state, created_at, completed_at)
             VALUES ($1, $2, $3, 'accepted', $4::text::timestamptz, $4::text::timestamptz) RETURNING generation",
        ).bind(&later_id).bind(guild).bind(user).bind(DUE).fetch_one(&pool).await.unwrap() };
        if temporary {
            sqlx::query(
                "INSERT INTO moderation_scheduled_unbans (request_id, guild_id, user_id, execute_at, reason, state, created_at)
                 VALUES ($1, $2, $3, $4::text::timestamptz, 'later expiry', 'staged', $4::text::timestamptz)",
            ).bind(&later_id).bind(guild).bind(user).bind(DUE).execute(&pool).await.unwrap();
        }
        let proof = HistoricalBanAcceptance {
            later_request_id: later_id.clone(),
            later_attempt: later,
            acceptance_evidence_id: "fixture:older-put-accepted".into(),
            ordering_evidence_id: "fixture:older-completed-before-later".into(),
            actor_id: "111111111111111111".into(),
        };
        let before = put_snapshot(&pool, &older_id).await;
        let later_before = put_snapshot(&pool, &later_id).await;
        assert!(store
            .confirm_ban_attempt(guild, user, &older_id, older, DUE)
            .await
            .is_err());
        assert_eq!(svc.run_due_unbans(guild).await.unwrap(), 0);
        for bad in 0..7 {
            let mut evidence = proof.clone();
            let mut attempt = older;
            let mut proof_guild = guild;
            let mut proof_user = user;
            match bad {
                0 => evidence.ordering_evidence_id.clear(),
                1 => evidence.acceptance_evidence_id.clear(),
                2 => evidence.later_attempt.generation += 1,
                3 => attempt.generation += 1,
                4 => evidence.later_request_id = "absent".into(),
                5 => proof_guild = "200000000000000002",
                _ => proof_user = "999999999999999999",
            }
            assert!(store
                .resolve_historical_ban_acceptance(
                    proof_guild,
                    proof_user,
                    &older_id,
                    attempt,
                    &evidence,
                    DUE
                )
                .await
                .is_err());
            assert_eq!(put_snapshot(&pool, &older_id).await, before);
            assert_eq!(put_snapshot(&pool, &later_id).await, later_before);
        }
        // Real database failure after the conditional acceptance update must
        // roll it back, together with all schedule changes.
        sqlx::raw_sql(
            "CREATE FUNCTION refuse_historical_audit() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN IF NEW.outcome = 'accepted_historical' THEN RAISE EXCEPTION 'fixture audit failure'; END IF;
             RETURN NEW; END $$;
             CREATE TRIGGER refuse_historical_audit BEFORE INSERT ON moderation_audit
             FOR EACH ROW EXECUTE FUNCTION refuse_historical_audit();",
        ).execute(&pool).await.unwrap();
        assert!(store
            .resolve_historical_ban_acceptance(guild, user, &older_id, older, &proof, DUE)
            .await
            .is_err());
        assert_eq!(put_snapshot(&pool, &older_id).await, before);
        assert_eq!(put_snapshot(&pool, &later_id).await, later_before);
        sqlx::raw_sql("DROP TRIGGER refuse_historical_audit ON moderation_audit; DROP FUNCTION refuse_historical_audit();")
            .execute(&pool).await.unwrap();
        store
            .serialize_member(guild, user, || {
                store.resolve_historical_ban_acceptance(guild, user, &older_id, older, &proof, DUE)
            })
            .await
            .unwrap();
        assert_eq!(ban_state(&pool, &older_id).await, "accepted");
        assert_eq!(unban_state(&pool, &older_id).await, "superseded");
        assert_eq!(put_snapshot(&pool, &later_id).await, later_before);
        let audit_id = format!("historical-ban:{}:{older_id}", older.generation);
        let (outcome, metadata): (String, String) = sqlx::query_as(
            "SELECT outcome, metadata_json FROM moderation_audit WHERE request_id = $1",
        )
        .bind(&audit_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(outcome, "accepted_historical");
        let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
        assert_eq!(metadata["ban_attempt_generation"], older.generation);
        assert_eq!(metadata["later_ban_attempt_generation"], later.generation);
        assert_eq!(metadata["ordering_evidence_id"], proof.ordering_evidence_id);
        assert_eq!(
            svc.run_due_unbans(guild).await.unwrap(),
            usize::from(temporary)
        );
        assert_eq!(discord.call_count("unban"), usize::from(temporary));
        discord.clear_failure("ban");
        let fresh_id = format!("historical-fresh-{user}");
        let mut fresh = execution(ModerationAction::Ban, &fresh_id);
        fresh.target.as_mut().unwrap().user_id = user.into();
        assert!(
            !svc.execute(&fresh)
                .await
                .expect("only older uncertainty was resolved")
                .replayed
        );
        assert_eq!(discord.call_count("ban"), 2);
        assert_eq!(ban_state(&pool, &fresh_id).await, "accepted");
    }
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_historical_acceptance_preserves_dispatched_delete_evidence() {
    use two_bot_core::member_moderation::{BanAttempt, HistoricalBanAcceptance};
    let (admin, pool, schema) = database().await;
    let guild = "100000000000000001";
    let user = "333333333333333333";
    let store = PgMemberModerationStore::new(pool.clone(), guild);
    let older = store
        .stage_unban(guild, user, DUE, "expiry", "older", NOW)
        .await
        .unwrap();
    let later = BanAttempt {
        generation: sqlx::query_scalar(
            "INSERT INTO moderation_member_bans (request_id, guild_id, user_id, state, created_at)
         VALUES ('later', $1, $2, 'accepted', $3::text::timestamptz) RETURNING generation",
        )
        .bind(guild)
        .bind(user)
        .bind(DUE)
        .fetch_one(&pool)
        .await
        .unwrap(),
    };
    sqlx::query(
        "UPDATE moderation_scheduled_unbans SET state = 'running', dispatch_uncertain = TRUE,
         claimed_at = $1::text::timestamptz, claim_token = 'fixture-delete-claim' WHERE request_id = 'older'",
    ).bind(DUE).execute(&pool).await.unwrap();
    let before: String = sqlx::query_scalar("SELECT row_to_json(job)::text FROM moderation_scheduled_unbans AS job WHERE request_id = 'older'")
        .fetch_one(&pool).await.unwrap();
    let proof = HistoricalBanAcceptance {
        later_request_id: "later".into(),
        later_attempt: later,
        acceptance_evidence_id: "fixture:older-put-accepted".into(),
        ordering_evidence_id: "fixture:older-completed-before-later".into(),
        actor_id: "111111111111111111".into(),
    };
    store
        .resolve_historical_ban_acceptance(guild, user, "older", older, &proof, DUE)
        .await
        .unwrap();
    let after: String = sqlx::query_scalar("SELECT row_to_json(job)::text FROM moderation_scheduled_unbans AS job WHERE request_id = 'older'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(before, after);
    assert!(store.stage_ban(guild, user, "fresh", DUE).await.is_err());
    assert!(store
        .claim_due_unbans(guild, DUE, 25)
        .await
        .unwrap()
        .is_empty());
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_stale_put_outcomes_cannot_resolve_a_retried_generation() {
    let (admin, pool, schema) = database().await;
    let guild = "100000000000000001";
    let store = PgMemberModerationStore::new(pool.clone(), guild);
    for (action, user) in [
        (ModerationAction::Ban, "333333333333333333"),
        (ModerationAction::TempBan, "444444444444444444"),
    ] {
        let clock = AtomicI64::new(1_700_000_000_000);
        let discord = MockMemberDiscord::new();
        let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
            clock.load(Ordering::SeqCst)
        });
        let old_id = format!("old-{user}");
        let mut old = execution(ModerationAction::TempBan, &old_id);
        old.target.as_mut().unwrap().user_id = user.into();
        svc.execute(&old).await.expect("older accepted expiry");
        let retry_id = format!("retry-{user}");
        let mut retry = execution(action, &retry_id);
        retry.target.as_mut().unwrap().user_id = user.into();
        discord.fail_with("ban", DiscordError::Rejected("definite refusal".into()));
        assert!(matches!(
            svc.execute(&retry).await,
            Err(two_bot_core::member_moderation::MemberError::Discord(
                DiscordError::Rejected(_)
            ))
        ));
        let first = fixture_attempt(&pool, &retry_id).await;
        assert_eq!(ban_state(&pool, &retry_id).await, "rejected");
        discord.fail_with("ban", DiscordError::Timeout);
        assert!(matches!(
            svc.execute(&retry).await,
            Err(two_bot_core::member_moderation::MemberError::Discord(
                DiscordError::Timeout
            ))
        ));
        let second = fixture_attempt(&pool, &retry_id).await;
        assert!(second.generation > first.generation);
        let before = put_snapshot(&pool, &retry_id).await;
        let older_before = put_snapshot(&pool, &old_id).await;
        for accepted in [false, true] {
            for identity_only in [false, true] {
                let result = match (accepted, identity_only) {
                    (false, false) => {
                        store
                            .reject_ban_attempt(guild, user, &retry_id, first, DUE)
                            .await
                    }
                    (true, false) => {
                        store
                            .confirm_ban_attempt(guild, user, &retry_id, first, DUE)
                            .await
                    }
                    (false, true) => store.reject_ban(guild, user, &retry_id, DUE).await,
                    (true, true) => store.confirm_ban(guild, user, &retry_id, DUE).await,
                };
                assert!(result.is_err(), "stale or missing attempt must fail closed");
                assert_eq!(put_snapshot(&pool, &retry_id).await, before);
                assert_eq!(put_snapshot(&pool, &old_id).await, older_before);
            }
        }
        clock.store(1_700_003_600_000, Ordering::SeqCst);
        assert_eq!(svc.run_due_unbans(guild).await.unwrap(), 0);
        assert_eq!(discord.call_count("unban"), 0);
        assert!(matches!(
            svc.execute(&retry).await,
            Err(two_bot_core::member_moderation::MemberError::InFlight)
        ));
        let mut fresh = execution(ModerationAction::Ban, &format!("fresh-{user}"));
        fresh.target.as_mut().unwrap().user_id = user.into();
        assert!(svc.execute(&fresh).await.is_err());
        assert_eq!(discord.call_count("ban"), 3);
        store
            .reject_ban_attempt(guild, user, &retry_id, second, DUE)
            .await
            .unwrap();
        assert_eq!(ban_state(&pool, &retry_id).await, "rejected");
        if action == ModerationAction::TempBan {
            assert_eq!(unban_state(&pool, &retry_id).await, "cancelled");
        }
        assert_eq!(svc.run_due_unbans(guild).await.unwrap(), 1);
        assert_eq!(discord.call_count("unban"), 1);
    }
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_staging_returns_the_committed_put_attempt() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    for temporary in [false, true] {
        let user = if temporary { "temporary" } else { "permanent" };
        let first = if temporary {
            store
                .stage_unban("guild", user, DUE, "expiry", user, NOW)
                .await
        } else {
            store.stage_ban("guild", user, user, NOW).await
        }
        .unwrap();
        assert_eq!(first.generation, generation(&pool, user).await);
        store
            .reject_ban_attempt("guild", user, user, first, NOW)
            .await
            .unwrap();
        let second = if temporary {
            store
                .stage_unban("guild", user, DUE, "retry expiry", user, NOW)
                .await
        } else {
            store.stage_ban("guild", user, user, NOW).await
        }
        .unwrap();
        assert!(second.generation > first.generation);
        let before = put_snapshot(&pool, user).await;
        assert!(store
            .confirm_ban_attempt("guild", user, user, first, DUE)
            .await
            .is_err());
        assert!(store
            .reject_ban_attempt("guild", user, user, first, DUE)
            .await
            .is_err());
        assert_eq!(put_snapshot(&pool, user).await, before);
        store
            .confirm_ban_attempt("guild", user, user, second, DUE)
            .await
            .unwrap();
        assert_eq!(ban_state(&pool, user).await, "accepted");
    }
    cleanup(admin, pool, schema).await;
}

fn execution(action: ModerationAction, id: &str) -> MemberExecution {
    MemberExecution {
        action,
        guild_id: "100000000000000001".into(),
        actor: ModerationActor {
            user_id: "111111111111111111".into(),
            role_ids: vec![],
            highest_role_position: 50,
            permissions: u64::MAX,
        },
        target: Some(ModerationTarget {
            user_id: "333333333333333333".into(),
            role_ids: vec![],
            highest_role_position: 10,
            is_bot: false,
            is_guild_owner: false,
        }),
        bot_highest_role_position: Some(100),
        reason: "spam".into(),
        duration_seconds: Some(3600),
        request_id: id.into(),
        idempotency_key: id.into(),
    }
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_member_ledger() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    let (a, b) = tokio::join!(
        store.claim("guild", "key", "moderation.ban", "hash", NOW),
        store.claim("guild", "key", "moderation.ban", "hash", NOW),
    );
    let states = [a.expect("claim A"), b.expect("claim B")];
    assert_eq!(
        states.iter().filter(|s| **s == ClaimState::Claimed).count(),
        1
    );
    assert_eq!(
        states
            .iter()
            .filter(|s| **s == ClaimState::InFlight)
            .count(),
        1
    );
    assert_eq!(
        store
            .claim("guild", "key", "moderation.ban", "other", DUE)
            .await
            .expect("mismatch"),
        ClaimState::Mismatch
    );
    store
        .complete("guild", "key", "banned", "{}", NOW)
        .await
        .expect("complete");
    store
        .release("guild", "key")
        .await
        .expect("done rows cannot be released");
    assert_eq!(
        store
            .claim("guild", "key", "moderation.ban", "hash", DUE)
            .await
            .expect("replay"),
        ClaimState::Replayed {
            outcome: "banned".into()
        }
    );
    assert!(store
        .complete("guild", "missing", "banned", "{}", NOW)
        .await
        .is_err());

    let clock = AtomicI64::new(1_700_000_000_000);
    let discord = MockMemberDiscord::new();
    let policy = ModerationPolicy {
        owen_user_id: "123456789012345678".into(),
        bot_user_id: Some("555555555555555555".into()),
        protected_role_ids: HashSet::new(),
    };
    let service_store = PgMemberModerationStore::new(pool.clone(), "100000000000000001");
    let svc = MemberModerationService::new(discord.clone(), service_store, policy, || {
        clock.load(Ordering::SeqCst)
    });
    for (action, id) in [
        (ModerationAction::Ban, "ban"),
        (ModerationAction::Kick, "kick"),
        (ModerationAction::Timeout, "timeout"),
        (ModerationAction::Warn, "warn"),
        (ModerationAction::TempBan, "tempban"),
    ] {
        let request = execution(action, id);
        assert!(!svc.execute(&request).await.expect("execute").replayed);
        assert!(svc.execute(&request).await.expect("replay").replayed);
    }
    assert_eq!(discord.calls().len(), 4);
    let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moderation_audit")
        .fetch_one(&pool)
        .await
        .expect("audits");
    assert_eq!(audits, 5);
    let warning = sqlx::query(
        "SELECT user_id, actor_id, reason FROM moderation_warnings WHERE request_id = 'warn'",
    )
    .fetch_one(&pool)
    .await
    .expect("warning");
    assert_eq!(warning.get::<String, _>("reason"), "spam");
    assert_eq!(warning.get::<String, _>("user_id"), "333333333333333333");
    assert_eq!(
        svc.run_due_unbans("100000000000000001")
            .await
            .expect("not due"),
        0
    );
    clock.store(1_700_003_600_000, Ordering::SeqCst);
    assert_eq!(
        svc.run_due_unbans("100000000000000001").await.expect("due"),
        1
    );
    assert_eq!(
        svc.run_due_unbans("100000000000000001")
            .await
            .expect("no duplicate"),
        0
    );
    assert_eq!(discord.call_count("unban"), 1);
    let audit = sqlx::query(
        "SELECT action, outcome, actor_id FROM moderation_audit WHERE request_id = 'tempban:unban'",
    )
    .fetch_one(&pool)
    .await
    .expect("unban audit");
    assert_eq!(
        audit.get::<String, _>("action"),
        "moderation.unban_scheduled"
    );
    assert_eq!(audit.get::<String, _>("actor_id"), "555555555555555555");
    assert_eq!(audit.get::<String, _>("outcome"), "unbanned");

    discord.fail_with("ban", DiscordError::Rejected("hierarchy".into()));
    let retry = execution(ModerationAction::TempBan, "rejected");
    assert!(svc.execute(&retry).await.is_err());
    discord.clear_failure("ban");
    assert!(
        !svc.execute(&retry)
            .await
            .expect("restage cancelled request")
            .replayed
    );

    // Accepted crash-left staged expiry is recovered. Parallel sweeps own it
    // once; old tokens cannot complete/requeue a new claim or steal a newer ban.
    let g2 = PgMemberModerationStore::new(pool.clone(), "g2");
    g2.stage_unban("g2", "u2", NOW, "expiry", "crash", NOW)
        .await
        .expect("stage");
    g2.confirm_ban_attempt(
        "g2",
        "u2",
        "crash",
        fixture_attempt(&pool, "crash").await,
        NOW,
    )
    .await
    .expect("Discord acceptance persisted before crash");
    let (a, b) = tokio::join!(
        g2.claim_due_unbans("g2", NOW, 25),
        g2.claim_due_unbans("g2", NOW, 25)
    );
    let jobs: Vec<_> = a
        .expect("sweep A")
        .into_iter()
        .chain(b.expect("sweep B"))
        .collect();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job.request_id, "crash");
    assert!(g2
        .complete_unban(&job.request_id, "wrong-token")
        .await
        .is_err());
    g2.requeue_unban(&job.request_id, &job.claim_token)
        .await
        .expect("safe retry");
    let next = g2
        .claim_due_unbans("g2", NOW, 25)
        .await
        .expect("new claim")
        .pop()
        .expect("job");
    assert_ne!(next.claim_token, job.claim_token);
    assert!(!g2
        .owns_unban_claim(&job.request_id, &job.claim_token)
        .await
        .expect("stale owner"));
    g2.requeue_unban(&job.request_id, &job.claim_token)
        .await
        .expect("stale requeue is harmless");
    assert!(g2
        .owns_unban_claim(&next.request_id, &next.claim_token)
        .await
        .expect("new claim unchanged"));
    assert!(g2
        .claim_due_unbans("g2", DUE, 25)
        .await
        .expect("no running takeover")
        .is_empty());
    assert!(g2
        .stage_unban("g2", "u2", DUE, "extension", "new", NOW)
        .await
        .is_err());
    g2.resolve_uncertain_unban(
        &next.request_id,
        &next.claim_token,
        two_bot_core::member_moderation::UnbanResolution::Void,
    )
    .await
    .expect("authoritative no-late-DELETE evidence clears the running fence");
    g2.stage_unban("g2", "u2", DUE, "extension", "new", NOW)
        .await
        .expect("new stage after reconciliation");
    // Even before acceptance, the newer prepared intent fences the old claim.
    assert!(!g2
        .owns_unban_claim(&next.request_id, &next.claim_token)
        .await
        .expect("prepared fence"));
    assert!(g2
        .activate_staged_unban("wrong-guild", "u2", "new", NOW)
        .await
        .is_err());
    assert!(g2
        .activate_staged_unban("g2", "u2", "new", NOW)
        .await
        .is_err());
    g2.confirm_ban_attempt("g2", "u2", "new", fixture_attempt(&pool, "new").await, NOW)
        .await
        .expect("new acceptance");
    g2.activate_staged_unban("g2", "u2", "new", NOW)
        .await
        .expect("new activation");
    assert!(!g2
        .owns_unban_claim(&next.request_id, &next.claim_token)
        .await
        .expect("superseded"));
    assert!(g2
        .complete_unban(&next.request_id, &next.claim_token)
        .await
        .is_err());
    assert_eq!(
        g2.claim_due_unbans("g2", DUE, 25)
            .await
            .expect("new expiry")
            .len(),
        1
    );

    let g3 = PgMemberModerationStore::new(pool.clone(), "g3");
    g3.stage_unban("g3", "u3", NOW, "old expiry", "old-staged", NOW)
        .await
        .expect("old crash");
    g3.confirm_ban_attempt(
        "g3",
        "u3",
        "old-staged",
        fixture_attempt(&pool, "old-staged").await,
        NOW,
    )
    .await
    .expect("old acceptance");
    g3.stage_unban(
        "g3",
        "u3",
        DUE,
        "extension",
        "new-staged",
        "2023-11-14T22:13:21.000Z",
    )
    .await
    .expect("new crash");
    g3.confirm_ban_attempt(
        "g3",
        "u3",
        "new-staged",
        fixture_attempt(&pool, "new-staged").await,
        NOW,
    )
    .await
    .expect("new acceptance");
    assert!(g3
        .claim_due_unbans("g3", NOW, 25)
        .await
        .expect("no early recovery")
        .is_empty());
    let recovered = g3
        .claim_due_unbans("g3", DUE, 25)
        .await
        .expect("latest expiry");
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].request_id, "new-staged");

    // Ledger failures after Discord never release the destructive claim.
    sqlx::query("DROP TABLE moderation_audit")
        .execute(&pool)
        .await
        .expect("inject scratch audit failure");
    let audit_loss = execution(ModerationAction::Kick, "audit-loss");
    assert!(
        !svc.execute(&audit_loss)
            .await
            .expect("mutation succeeds despite audit loss")
            .replayed
    );
    assert!(
        svc.execute(&audit_loss)
            .await
            .expect("replay after audit loss")
            .replayed
    );
    sqlx::query(
        "ALTER TABLE moderation_idempotency ADD CONSTRAINT test_refuse_completion
                 CHECK (state <> 'done') NOT VALID",
    )
    .execute(&pool)
    .await
    .expect("inject scratch completion failure");
    let completion_loss = execution(ModerationAction::Kick, "completion-loss");
    assert!(svc.execute(&completion_loss).await.is_err());
    assert!(matches!(
        svc.execute(&completion_loss).await,
        Err(two_bot_core::member_moderation::MemberError::InFlight)
    ));
    assert_eq!(discord.call_count("kick"), 3); // ordinary kick + each failure case once

    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_guild_bound_sweep_and_write_fences() {
    let (admin, pool, schema) = database().await;
    let g1 = PgMemberModerationStore::new(pool.clone(), "g1");
    let g2 = PgMemberModerationStore::new(pool.clone(), "g2");
    accepted_unban(&g1, "g1", "same-member", "own-expiry", NOW, NOW).await;
    accepted_unban(&g2, "g2", "expiry-member", "foreign-expiry", NOW, NOW).await;
    accepted_unban(&g2, "g2", "other-member", "foreign-running", NOW, NOW).await;
    let foreign_jobs = g2
        .claim_due_unbans("g2", NOW, 25)
        .await
        .expect("foreign claims");
    let foreign_job = foreign_jobs
        .iter()
        .find(|job| job.request_id == "foreign-running")
        .expect("foreign running job");
    // Another accepted crash-left row must not be activated by g1's sweep.
    accepted_unban(&g2, "g2", "crash-member", "foreign-staged", NOW, NOW).await;

    let ready = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let held_store = g2.clone();
    let held_ready = ready.clone();
    let held_release = release.clone();
    let held = tokio::spawn(async move {
        held_store
            .serialize_member("g2", "same-member", || async {
                held_store
                    .stage_ban("g2", "same-member", "foreign-ban-held", NOW)
                    .await
                    .expect("prepare foreign ban before waiting for Discord");
                held_ready.notify_one();
                held_release.notified().await;
            })
            .await;
    });
    tokio::time::timeout(Duration::from_secs(2), ready.notified())
        .await
        .expect("foreign task must acquire its queue without panicking");
    let discord = MockMemberDiscord::new();
    let svc =
        MemberModerationService::new(discord.clone(), g1.clone(), policy(), || 1_700_003_600_000);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), svc.run_due_unbans("g1"))
            .await
            .expect("own guild cannot wait behind foreign member")
            .expect("own sweep"),
        1
    );
    assert_eq!(discord.call_count("unban"), 1);
    assert_eq!(unban_state(&pool, "own-expiry").await, "done");
    assert_eq!(unban_state(&pool, "foreign-staged").await, "staged");
    assert_eq!(unban_state(&pool, "foreign-expiry").await, "running");
    assert!(g2
        .owns_unban_claim(
            &foreign_jobs
                .iter()
                .find(|job| job.request_id == "foreign-expiry")
                .expect("expiry")
                .request_id,
            &foreign_jobs
                .iter()
                .find(|job| job.request_id == "foreign-expiry")
                .expect("expiry")
                .claim_token,
        )
        .await
        .expect("own sweep leaves the foreign running claim unchanged"));

    // Guild-taking entry points reject before any insert, update or delete.
    g2.claim("g2", "foreign-key", "moderation.ban", "hash", NOW)
        .await
        .expect("foreign key");
    assert!(g1
        .claim("g2", "new-key", "moderation.ban", "hash", NOW)
        .await
        .is_err());
    assert!(g1
        .complete("g2", "foreign-key", "banned", "{}", NOW)
        .await
        .is_err());
    assert!(g1.release("g2", "foreign-key").await.is_err());
    assert!(g1
        .add_warning(
            "foreign-warning",
            "g2",
            "user",
            "actor",
            "reason",
            "foreign-warning",
            NOW
        )
        .await
        .is_err());
    assert!(g1
        .stage_unban("g2", "user", NOW, "reason", "foreign-new", NOW)
        .await
        .is_err());
    assert!(g1
        .stage_ban("g2", "user", "foreign-new", NOW)
        .await
        .is_err());
    assert!(g1
        .confirm_ban_attempt(
            "g2",
            "same-member",
            "foreign-ban-held",
            fixture_attempt(&pool, "foreign-ban-held").await,
            NOW
        )
        .await
        .is_err());
    assert!(g1
        .reject_ban_attempt(
            "g2",
            "same-member",
            "foreign-ban-held",
            fixture_attempt(&pool, "foreign-ban-held").await,
            NOW
        )
        .await
        .is_err());
    assert!(g1
        .activate_staged_unban("g2", "crash-member", "foreign-staged", NOW)
        .await
        .is_err());
    assert!(g1.claim_due_unbans("g2", NOW, 25).await.is_err());
    let audit = AuditRow {
        request_id: "foreign-audit".into(),
        guild_id: "g2".into(),
        actor_id: "actor".into(),
        action: "moderation.ban",
        target_id: Some("user".into()),
        reason: "reason".into(),
        outcome: "banned",
        idempotency_key: "foreign-audit".into(),
        metadata_json: "{}".into(),
    };
    assert!(g1.record_audit(&audit).await.is_err());
    // serialize_member has a generic output; the write inside still fences.
    assert!(g1
        .serialize_member("g2", "user", || g1.stage_ban(
            "g2",
            "user",
            "inside-queue",
            NOW
        ))
        .await
        .is_err());
    assert!(!g1
        .owns_unban_claim(&foreign_job.request_id, &foreign_job.claim_token)
        .await
        .expect("foreign ownership"));
    assert!(g1
        .complete_unban(&foreign_job.request_id, &foreign_job.claim_token)
        .await
        .is_err());
    g1.requeue_unban(&foreign_job.request_id, &foreign_job.claim_token)
        .await
        .expect("foreign requeue cannot write");
    assert!(g2
        .owns_unban_claim(&foreign_job.request_id, &foreign_job.claim_token)
        .await
        .expect("foreign token unchanged"));
    assert_eq!(ban_state(&pool, "foreign-ban-held").await, "prepared");
    assert_eq!(unban_state(&pool, "foreign-staged").await, "staged");
    assert_eq!(
        g2.claim("g2", "foreign-key", "moderation.ban", "hash", NOW)
            .await
            .expect("key preserved"),
        ClaimState::InFlight
    );
    let foreign_new: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moderation_member_bans WHERE request_id IN ('foreign-new', 'inside-queue')")
        .fetch_one(&pool).await.expect("no foreign insert");
    assert_eq!(foreign_new, 0);
    let warnings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moderation_warnings")
        .fetch_one(&pool)
        .await
        .expect("no foreign warning");
    assert_eq!(warnings, 0);
    let foreign_audits: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM moderation_audit WHERE guild_id = 'g2'")
            .fetch_one(&pool)
            .await
            .expect("no foreign audit");
    assert_eq!(foreign_audits, 0);
    release.notify_one();
    held.await.expect("foreign consumer released");
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_rejection_write_failure_keeps_permanent_ban_fenced() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "100000000000000001");
    let clock = AtomicI64::new(1_700_000_000_000);
    let discord = MockMemberDiscord::new();
    let svc = MemberModerationService::new(discord.clone(), store, policy(), || {
        clock.load(Ordering::SeqCst)
    });
    svc.execute(&execution(ModerationAction::Ban, "permanent"))
        .await
        .expect("permanent ban");
    sqlx::query("ALTER TABLE moderation_scheduled_unbans ADD CONSTRAINT test_refuse_rejection CHECK (request_id <> 'rejected-temp' OR state <> 'cancelled') NOT VALID")
        .execute(&pool).await.expect("inject scratch cancellation failure");
    discord.fail_with("ban", DiscordError::Rejected("hierarchy".into()));
    assert!(svc
        .execute(&execution(ModerationAction::TempBan, "rejected-temp"))
        .await
        .is_err());
    assert_eq!(ban_state(&pool, "permanent").await, "accepted");
    assert_eq!(ban_state(&pool, "rejected-temp").await, "prepared");
    assert_eq!(unban_state(&pool, "rejected-temp").await, "staged");
    discord.clear_failure("ban");
    clock.store(1_700_003_600_000, Ordering::SeqCst);
    for _ in 0..2 {
        assert_eq!(
            svc.run_due_unbans("100000000000000001")
                .await
                .expect("uncertain rejection cannot expire"),
            0
        );
    }
    assert_eq!(discord.call_count("unban"), 0);
    assert_eq!(unban_state(&pool, "rejected-temp").await, "staged");
    assert!(matches!(
        svc.execute(&execution(ModerationAction::TempBan, "rejected-temp"))
            .await,
        Err(two_bot_core::member_moderation::MemberError::InFlight)
    ));
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_generation_wins_over_timestamp_and_confirmation_order() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    for (user, created) in [("tie", NOW), ("backward", "2023-11-14T21:13:20.000Z")] {
        let old = format!("z-old-{user}");
        let new = format!("a-new-{user}");
        accepted_unban(&store, "guild", user, &old, NOW, NOW).await;
        accepted_unban(&store, "guild", user, &new, DUE, created).await;
        assert!(generation(&pool, &new).await > generation(&pool, &old).await);
        assert_eq!(unban_state(&pool, &old).await, "superseded");
    }
    assert!(store
        .claim_due_unbans("guild", NOW, 25)
        .await
        .expect("old expiries cannot fire")
        .is_empty());
    let jobs = store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("generation-ordered recovery");
    assert_eq!(jobs.len(), 2);
    assert_eq!(
        jobs.into_iter()
            .map(|job| job.request_id)
            .collect::<HashSet<_>>(),
        HashSet::from(["a-new-tie".to_owned(), "a-new-backward".to_owned()])
    );

    // New staging now refuses unresolved old PUTs. An inconsistent imported
    // ledger must also fence the newer expiry until the exact old PUT resolves.
    store
        .stage_unban(
            "guild",
            "late-confirm",
            NOW,
            "old",
            "older-late-confirm",
            DUE,
        )
        .await
        .expect("older prepared");
    assert!(store
        .stage_ban("guild", "late-confirm", "newer-first-confirm", NOW)
        .await
        .is_err());
    sqlx::query("INSERT INTO moderation_member_bans (request_id, guild_id, user_id, state, created_at) VALUES ('newer-first-confirm', 'guild', 'late-confirm', 'accepted', $1::text::timestamptz)")
        .bind(NOW).execute(&pool).await.expect("historical newer acceptance");
    sqlx::query("INSERT INTO moderation_scheduled_unbans (request_id, guild_id, user_id, execute_at, reason, state, created_at) VALUES ('newer-first-confirm', 'guild', 'late-confirm', $1::text::timestamptz, 'expiry', 'staged', $1::text::timestamptz)")
        .bind(NOW).execute(&pool).await.expect("historical newer expiry");
    assert!(store
        .confirm_ban_attempt(
            "guild",
            "late-confirm",
            "older-late-confirm",
            fixture_attempt(&pool, "older-late-confirm").await,
            DUE
        )
        .await
        .is_err());
    assert!(store
        .activate_staged_unban("guild", "late-confirm", "newer-first-confirm", NOW)
        .await
        .is_err());
    assert!(store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("older PUT fences recovery")
        .is_empty());
    assert_eq!(ban_state(&pool, "older-late-confirm").await, "prepared");
    assert_eq!(unban_state(&pool, "older-late-confirm").await, "staged");
    store
        .reject_ban_attempt(
            "guild",
            "late-confirm",
            "older-late-confirm",
            fixture_attempt(&pool, "older-late-confirm").await,
            DUE,
        )
        .await
        .expect("authoritative old PUT refusal");
    let jobs = store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("newer recovers after proof");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].request_id, "newer-first-confirm");
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_permanent_ban_supersession_and_failed_confirmation_fence() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    accepted_unban(&store, "guild", "pending-user", "pending-expiry", NOW, NOW).await;
    accepted_unban(&store, "guild", "running-user", "running-expiry", NOW, NOW).await;
    store
        .activate_staged_unban("guild", "pending-user", "pending-expiry", NOW)
        .await
        .expect("pending expiry");
    store
        .activate_staged_unban("guild", "running-user", "running-expiry", NOW)
        .await
        .expect("other pending expiry");
    // Claim only the running user's job, without leaving an accidental claim
    // for the pending user's independently tested fence.
    sqlx::query("UPDATE moderation_scheduled_unbans SET execute_at = $1::text::timestamptz WHERE request_id = 'pending-expiry'")
        .bind(DUE).execute(&pool).await.expect("pending later");
    let running = store
        .claim_due_unbans("guild", NOW, 1)
        .await
        .expect("one claim")
        .pop()
        .expect("running job");
    assert_eq!(running.request_id, "running-expiry");
    sqlx::query("ALTER TABLE moderation_scheduled_unbans ADD CONSTRAINT test_refuse_supersession CHECK (state <> 'superseded') NOT VALID")
        .execute(&pool).await.expect("inject scratch confirmation write failure");
    store
        .stage_ban("guild", "pending-user", "permanent-pending", NOW)
        .await
        .expect("permanent prepared before Discord");
    let error = store
        .confirm_ban_attempt(
            "guild",
            "pending-user",
            "permanent-pending",
            fixture_attempt(&pool, "permanent-pending").await,
            NOW,
        )
        .await
        .expect_err("confirmation rollback");
    assert_eq!(error.message, "postgres moderation ledger: 23514");
    assert_eq!(ban_state(&pool, "permanent-pending").await, "prepared");
    // Running claims now refuse staging altogether, before any intent/PUT.
    assert!(store
        .stage_ban("guild", "running-user", "permanent-running", NOW)
        .await
        .is_err());
    // Inject a pre-existing prepared intent to check late confirmation cannot
    // erase a dispatched claim, even on an inconsistent imported ledger.
    sqlx::query("INSERT INTO moderation_member_bans (request_id, guild_id, user_id, state, created_at) VALUES ('permanent-running', 'guild', 'running-user', 'prepared', $1::text::timestamptz)")
        .bind(NOW).execute(&pool).await.expect("scratch late intent");
    store
        .confirm_ban_attempt(
            "guild",
            "running-user",
            "permanent-running",
            fixture_attempt(&pool, "permanent-running").await,
            NOW,
        )
        .await
        .expect("running row is not superseded by confirmation");
    assert_eq!(unban_state(&pool, "pending-expiry").await, "pending");
    assert_eq!(unban_state(&pool, "running-expiry").await, "running");
    assert!(!store
        .owns_unban_claim(&running.request_id, &running.claim_token)
        .await
        .expect("prepared permanent fences running expiry"));
    assert!(store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("prepared permanent fences pending expiry")
        .is_empty());
    sqlx::query("ALTER TABLE moderation_scheduled_unbans DROP CONSTRAINT test_refuse_supersession")
        .execute(&pool)
        .await
        .expect("allow explicit reconciliation");
    store
        .confirm_ban_attempt(
            "guild",
            "pending-user",
            "permanent-pending",
            fixture_attempt(&pool, "permanent-pending").await,
            DUE,
        )
        .await
        .expect("reconciled permanent acceptance");
    assert_eq!(ban_state(&pool, "permanent-pending").await, "accepted");
    assert_eq!(unban_state(&pool, "pending-expiry").await, "superseded");
    assert_eq!(ban_state(&pool, "permanent-running").await, "accepted");
    assert_eq!(unban_state(&pool, "running-expiry").await, "running");
    let retained: String = sqlx::query_scalar(
        "SELECT claim_token FROM moderation_scheduled_unbans WHERE request_id = 'running-expiry'",
    )
    .fetch_one(&pool)
    .await
    .expect("uncertain token preserved");
    assert_eq!(retained, running.claim_token);
    assert!(store
        .stage_ban("guild", "running-user", "still-fenced", DUE)
        .await
        .is_err());
    assert!(store
        .resolve_uncertain_unban(
            &running.request_id,
            "wrong-token",
            two_bot_core::member_moderation::UnbanResolution::Void
        )
        .await
        .is_err());
    store
        .resolve_uncertain_unban(
            &running.request_id,
            &running.claim_token,
            two_bot_core::member_moderation::UnbanResolution::Void,
        )
        .await
        .expect("only authoritative reconciliation closes dispatched uncertainty");
    assert_eq!(unban_state(&pool, "running-expiry").await, "superseded");
    assert!(store
        .complete_unban(&running.request_id, &running.claim_token)
        .await
        .is_err());
    assert!(store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("no permanent expiry")
        .is_empty());
    let schedules: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moderation_scheduled_unbans WHERE request_id LIKE 'permanent-%'",
    )
    .fetch_one(&pool)
    .await
    .expect("permanent bans create no schedule");
    assert_eq!(schedules, 0);
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_stage_atomicity_and_rejected_retry_generation() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    sqlx::query("ALTER TABLE moderation_scheduled_unbans ADD CONSTRAINT test_refuse_stage CHECK (request_id <> 'stage-loss')")
        .execute(&pool).await.expect("inject scratch stage insert failure");
    assert!(store
        .stage_unban("guild", "user", NOW, "private reason", "stage-loss", NOW)
        .await
        .is_err());
    let leaked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moderation_member_bans WHERE request_id = 'stage-loss'",
    )
    .fetch_one(&pool)
    .await
    .expect("atomic stage rollback");
    assert_eq!(leaked, 0);
    store
        .stage_unban("guild", "user", NOW, "expiry", "retry", NOW)
        .await
        .expect("first prepare");
    let first = generation(&pool, "retry").await;
    assert!(store
        .stage_ban("guild", "user", "retry", NOW)
        .await
        .is_err());
    assert!(store
        .confirm_ban_attempt(
            "guild",
            "wrong-user",
            "retry",
            fixture_attempt(&pool, "retry").await,
            NOW
        )
        .await
        .is_err());
    assert!(store
        .reject_ban_attempt(
            "guild",
            "wrong-user",
            "retry",
            fixture_attempt(&pool, "retry").await,
            NOW
        )
        .await
        .is_err());
    assert!(store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("prepared is never recovered")
        .is_empty());
    store
        .reject_ban_attempt(
            "guild",
            "user",
            "retry",
            fixture_attempt(&pool, "retry").await,
            NOW,
        )
        .await
        .expect("safe rejection");
    assert_eq!(ban_state(&pool, "retry").await, "rejected");
    assert_eq!(unban_state(&pool, "retry").await, "cancelled");
    assert!(store
        .stage_unban("guild", "wrong-user", NOW, "expiry", "retry", NOW)
        .await
        .is_err());
    assert_eq!(generation(&pool, "retry").await, first);
    store
        .stage_unban(
            "guild",
            "user",
            DUE,
            "retry expiry",
            "retry",
            "2023-11-14T21:13:20.000Z",
        )
        .await
        .expect("rejected exact identity may retry");
    assert!(generation(&pool, "retry").await > first);
    assert_eq!(ban_state(&pool, "retry").await, "prepared");
    assert!(store
        .activate_staged_unban("guild", "user", "retry", NOW)
        .await
        .is_err());
    store
        .confirm_ban_attempt(
            "guild",
            "user",
            "retry",
            fixture_attempt(&pool, "retry").await,
            NOW,
        )
        .await
        .expect("retried acceptance");
    assert!(store
        .stage_ban("guild", "user", "retry", NOW)
        .await
        .is_err());
    assert!(store
        .reject_ban_attempt(
            "guild",
            "user",
            "retry",
            fixture_attempt(&pool, "retry").await,
            NOW
        )
        .await
        .is_err());
    assert_eq!(
        store
            .claim_due_unbans("guild", DUE, 25)
            .await
            .expect("accepted retry recovers")
            .len(),
        1
    );

    // A safely rejected newer intent removes its fence, not the older valid
    // expiry; the rejected staged job itself can never become pending.
    accepted_unban(&store, "guild", "safe-reject", "safe-old", NOW, NOW).await;
    store
        .stage_unban("guild", "safe-reject", NOW, "new", "safe-new", NOW)
        .await
        .expect("new prepare");
    assert!(store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("prepared fence")
        .is_empty());
    store
        .reject_ban_attempt(
            "guild",
            "safe-reject",
            "safe-new",
            fixture_attempt(&pool, "safe-new").await,
            NOW,
        )
        .await
        .expect("safe rejection removes fence");
    let jobs = store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("older accepted expiry becomes eligible");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].request_id, "safe-old");
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_old_prepared_put_fences_new_staging_and_newer_accepted_expiry() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    store
        .stage_ban("guild", "user", "old-put", NOW)
        .await
        .expect("uncertain old PUT");
    let permanent = store.stage_ban("guild", "user", "new-put", DUE).await;
    let temporary = store
        .stage_unban("guild", "user", DUE, "expiry", "new-temp", DUE)
        .await;
    // Model a pre-repair/imported ledger with an older uncertain operation
    // and a newer accepted tempban. Generation alone is not remote ordering.
    sqlx::raw_sql("DELETE FROM moderation_scheduled_unbans WHERE request_id = 'new-temp'; DELETE FROM moderation_member_bans WHERE request_id IN ('new-put', 'new-temp');")
        .execute(&pool).await.expect("remove probe intents only");
    sqlx::query("INSERT INTO moderation_member_bans (request_id, guild_id, user_id, state, created_at) VALUES ('historical-new-temp', 'guild', 'user', 'accepted', $1::text::timestamptz)")
        .bind(NOW).execute(&pool).await.expect("historical accepted newer PUT");
    sqlx::query("INSERT INTO moderation_scheduled_unbans (request_id, guild_id, user_id, execute_at, reason, state, created_at) VALUES ('historical-new-temp', 'guild', 'user', $1::text::timestamptz, 'expiry', 'pending', $1::text::timestamptz)")
        .bind(NOW).execute(&pool).await.expect("historical expiry");
    let jobs = store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("uncertainty check");
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM moderation_member_bans WHERE state = 'prepared'")
            .fetch_one(&pool)
            .await
            .expect("old fence remains");
    cleanup(admin, pool, schema).await;
    assert!(permanent
        .expect_err("fresh permanent must refuse")
        .is_safe_pre_mutation());
    assert!(temporary
        .expect_err("fresh tempban must refuse")
        .is_safe_pre_mutation());
    assert_eq!(count, 1);
    assert!(
        jobs.is_empty(),
        "no DELETE may race an older unfinished PUT"
    );
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_quarantine_preserves_imported_delete_fences() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    for (id, state, token) in [
        ("running", "running", Some("old-token")),
        ("tokenless", "running", None),
        (
            "already-quarantined",
            "quarantined",
            Some("historical-token"),
        ),
    ] {
        sqlx::query("INSERT INTO moderation_scheduled_unbans (request_id, guild_id, user_id, execute_at, reason, state, created_at, claim_token) VALUES ($1, 'guild', $1, $2::text::timestamptz, 'imported expiry', $3, $2::text::timestamptz, $4)")
            .bind(id).bind(NOW).bind(state).bind(token).execute(&pool).await.expect("imported uncertain DELETE");
    }
    for _ in 0..2 {
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0111_moderation_ban_ownership.sql"
        ))
        .execute(&pool)
        .await
        .expect("repeat-safe quarantine");
    }
    let mut refused = Vec::new();
    for id in ["running", "tokenless", "already-quarantined"] {
        assert_eq!(unban_state(&pool, id).await, "quarantined");
        assert!(!store
            .owns_unban_claim(id, "old-token")
            .await
            .expect("imports non-executable"));
        refused.push(
            store
                .stage_ban("guild", id, &format!("new-{id}"), DUE)
                .await
                .is_err(),
        );
        refused.push(
            store
                .stage_unban("guild", id, DUE, "new expiry", &format!("temp-{id}"), DUE)
                .await
                .is_err(),
        );
    }
    let due = store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("imports inert");
    use two_bot_core::member_moderation::UnbanResolution;
    assert!(store
        .resolve_uncertain_unban("running", "wrong", UnbanResolution::Completed)
        .await
        .is_err());
    assert!(PgMemberModerationStore::new(pool.clone(), "other-guild")
        .resolve_uncertain_unban("running", "old-token", UnbanResolution::Completed)
        .await
        .is_err());
    store
        .resolve_uncertain_unban("running", "old-token", UnbanResolution::Completed)
        .await
        .expect("exact imported DELETE completed");
    store
        .resolve_uncertain_unban(
            "already-quarantined",
            "historical-token",
            UnbanResolution::Void,
        )
        .await
        .expect("exact imported DELETE cannot land");
    assert_eq!(unban_state(&pool, "running").await, "done");
    assert_eq!(
        unban_state(&pool, "already-quarantined").await,
        "quarantined",
        "void dispatch does not invent or cancel imported expiry ownership"
    );
    assert!(store
        .resolve_uncertain_unban("running", "old-token", UnbanResolution::Completed)
        .await
        .is_err());
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0111_moderation_ban_ownership.sql"
    ))
    .execute(&pool)
    .await
    .expect("replay after resolution");
    store
        .stage_ban("guild", "running", "after-completed", DUE)
        .await
        .expect("completed imported fence cleared");
    store
        .stage_ban("guild", "already-quarantined", "after-void", DUE)
        .await
        .expect("void imported fence cleared, quarantine remains inert");
    assert!(store
        .stage_ban("guild", "tokenless", "still-fenced", DUE)
        .await
        .is_err());
    let inferred: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moderation_member_bans WHERE request_id IN ('running', 'tokenless', 'already-quarantined')")
        .fetch_one(&pool).await.expect("no acceptance invented");
    cleanup(admin, pool, schema).await;
    assert_eq!(inferred, 0);
    assert!(due.is_empty());
    assert!(
        refused.into_iter().all(|r| r),
        "quarantine must not drop uncertain DELETE fences, even without a token"
    );
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_imported_unbans_are_quarantined_without_acceptance_inference() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    accepted_unban(&store, "guild", "trusted", "accepted-staged", NOW, NOW).await;
    store
        .stage_unban(
            "guild",
            "uncertain",
            NOW,
            "uncertain",
            "prepared-staged",
            NOW,
        )
        .await
        .expect("uncertain intent");
    for state in ["staged", "pending", "running"] {
        sqlx::query("INSERT INTO moderation_scheduled_unbans (request_id, guild_id, user_id, execute_at, reason, state, created_at, claimed_at, claim_token) VALUES ($1, 'guild', $1, $2::text::timestamptz, 'imported reason', $3, $2::text::timestamptz, $2::text::timestamptz, 'imported-token')")
            .bind(format!("imported-{state}")).bind(NOW).bind(state).execute(&pool).await.expect("import old schedule without trustworthy intent");
    }
    for _ in 0..2 {
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0111_moderation_ban_ownership.sql"
        ))
        .execute(&pool)
        .await
        .expect("idempotent migration over imported rows");
    }
    for state in ["staged", "pending", "running"] {
        let request = format!("imported-{state}");
        assert_eq!(unban_state(&pool, &request).await, "quarantined");
        let row = sqlx::query("SELECT reason, claim_token, completed_at IS NULL AS unfinished, claimed_at = $2::text::timestamptz AS claimed_preserved, created_at = $2::text::timestamptz AS created_preserved FROM moderation_scheduled_unbans WHERE request_id = $1")
            .bind(&request).bind(NOW).fetch_one(&pool).await.expect("quarantine preserves reconciliation evidence");
        assert_eq!(row.get::<String, _>("reason"), "imported reason");
        assert_eq!(row.get::<String, _>("claim_token"), "imported-token");
        assert!(row.get::<bool, _>("unfinished"));
        assert!(row.get::<bool, _>("claimed_preserved"));
        assert!(row.get::<bool, _>("created_preserved"));
        assert!(!store
            .owns_unban_claim(&request, "imported-token")
            .await
            .expect("untrusted ownership"));
    }
    assert_eq!(unban_state(&pool, "prepared-staged").await, "staged");
    assert_eq!(ban_state(&pool, "prepared-staged").await, "prepared");
    let inferred: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moderation_member_bans WHERE request_id LIKE 'imported-%'",
    )
    .fetch_one(&pool)
    .await
    .expect("no acceptance inferred");
    assert_eq!(inferred, 0);
    let jobs = store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("only explicit accepted intent recovers");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].request_id, "accepted-staged");
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_legacy_text_timestamps_upgrade_without_reactivating_imports() {
    let (admin, pool, schema) = database_schema().await;
    sqlx::raw_sql(include_str!("fixtures/member_moderation_legacy.sql"))
        .execute(&pool)
        .await
        .expect("legacy-shaped schema with eight TEXT timestamp columns");
    for state in ["staged", "pending", "running", "done"] {
        sqlx::query("INSERT INTO moderation_scheduled_unbans (request_id, guild_id, user_id, execute_at, reason, state, created_at, completed_at, claimed_at, claim_token) VALUES ($1, 'guild', $1, $2, 'preserved reason', $3, $2, $4, $2, 'preserved-token')")
            .bind(format!("legacy-{state}"))
            .bind(NOW)
            .bind(state)
            .bind((state == "done").then_some(DUE))
            .execute(&pool).await.expect("imported schedule");
    }
    sqlx::query("INSERT INTO moderation_warnings (id, guild_id, user_id, actor_id, reason, request_id, created_at) VALUES ('w', 'guild', 'user', 'actor', 'reason', 'w', $1)")
        .bind(NOW).execute(&pool).await.expect("legacy warning");
    sqlx::query("INSERT INTO moderation_audit (request_id, guild_id, actor_id, action, reason, outcome, idempotency_key, metadata_json, created_at) VALUES ('a', 'guild', 'actor', 'moderation.warn', 'reason', 'warned', 'a', '{}', $1)")
        .bind(NOW).execute(&pool).await.expect("legacy audit");
    sqlx::query("INSERT INTO moderation_idempotency (guild_id, idempotency_key, action, request_hash, state, outcome, result_json, claimed_at, completed_at) VALUES ('guild', 'key', 'moderation.warn', 'hash', 'done', 'warned', '{}', $1, $2)")
        .bind(NOW).bind(DUE).execute(&pool).await.expect("legacy completed claim");
    for migration in [
        include_str!("../../cutover/migrations/0110_moderation_member.sql"),
        include_str!("../../cutover/migrations/0111_moderation_ban_ownership.sql"),
        include_str!("../../cutover/migrations/0113_moderation_unban_retry_order.sql"),
    ] {
        sqlx::raw_sql(migration)
            .execute(&pool)
            .await
            .expect("pre-upgrade migrations");
    }
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    let error = store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect_err("quarantine alone cannot fix TEXT timestamp comparisons");
    assert_eq!(error.message, "postgres moderation ledger: 42883");
    for _ in 0..2 {
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0112_moderation_legacy_timestamps.sql"
        ))
        .execute(&pool)
        .await
        .expect("repeat-safe TEXT upgrade");
    }
    let timestamps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.columns WHERE table_schema = $1 AND table_name IN ('moderation_warnings', 'moderation_scheduled_unbans', 'moderation_audit', 'moderation_idempotency') AND data_type = 'timestamp with time zone'")
        .bind(&schema).fetch_one(&pool).await.expect("all legacy timestamp types upgraded");
    assert_eq!(timestamps, 8);
    for state in ["staged", "pending", "running", "done"] {
        let request = format!("legacy-{state}");
        assert_eq!(
            unban_state(&pool, &request).await,
            if state == "done" {
                "done"
            } else {
                "quarantined"
            }
        );
        let row = sqlx::query("SELECT reason, claim_token, execute_at = $2::text::timestamptz AND created_at = $2::text::timestamptz AND claimed_at = $2::text::timestamptz AS preserved, completed_at = $3::text::timestamptz AS completed FROM moderation_scheduled_unbans WHERE request_id = $1")
            .bind(&request).bind(NOW).bind(DUE).fetch_one(&pool).await.expect("timestamp and evidence preservation");
        assert_eq!(row.get::<String, _>("reason"), "preserved reason");
        assert_eq!(row.get::<String, _>("claim_token"), "preserved-token");
        assert!(row.get::<bool, _>("preserved"));
        assert_eq!(
            row.get::<Option<bool>, _>("completed"),
            (state == "done").then_some(true)
        );
    }
    let preserved: bool = sqlx::query_scalar("SELECT (SELECT created_at = $1::text::timestamptz FROM moderation_warnings WHERE id = 'w') AND (SELECT created_at = $1::text::timestamptz FROM moderation_audit WHERE request_id = 'a') AND (SELECT claimed_at = $1::text::timestamptz AND completed_at = $2::text::timestamptz FROM moderation_idempotency WHERE idempotency_key = 'key')")
        .bind(NOW).bind(DUE).fetch_one(&pool).await.expect("all ledgers retain their timestamps");
    assert!(preserved);
    assert!(store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("quarantined imports stay inert")
        .is_empty());
    assert_eq!(
        store
            .claim("guild", "key", "moderation.warn", "hash", NOW)
            .await
            .expect("legacy replay"),
        ClaimState::Replayed {
            outcome: "warned".into()
        }
    );
    accepted_unban(&store, "guild", "fresh-user", "fresh", NOW, NOW).await;
    let jobs = store
        .claim_due_unbans("guild", DUE, 25)
        .await
        .expect("fresh typed schedule works");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].request_id, "fresh");
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_staging_pool_timeout_is_mutation_free_in_both_paths() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    for temporary in [false, true] {
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(pool.acquire().await.unwrap());
        }
        let error = if temporary {
            store
                .stage_unban("guild", "member", DUE, "expiry", "pool-loss", NOW)
                .await
        } else {
            store.stage_ban("guild", "member", "pool-loss", NOW).await
        }
        .expect_err("pool is deliberately exhausted before staging begins");
        assert!(error.is_safe_pre_mutation());
        assert!(error.message.contains("pool_timeout"));
        drop(held);
        let rows: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM moderation_member_bans) +
                    (SELECT COUNT(*) FROM moderation_scheduled_unbans)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "acquisition failure leaves no intent or expiry");
    }
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_read_only_staging_timeout_releases_service_key_and_retries() {
    for action in [ModerationAction::Ban, ModerationAction::TempBan] {
        let (admin, pool, schema) = database().await;
        // Set the same timeout on every connection in this isolated pool.
        let mut held = Vec::new();
        for _ in 0..4 {
            let mut connection = pool.acquire().await.unwrap();
            sqlx::query("SET statement_timeout = '250ms'")
                .execute(&mut *connection)
                .await
                .unwrap();
            held.push(connection);
        }
        drop(held);
        let mut locker = admin.begin().await.unwrap();
        QueryBuilder::<Postgres>::new("LOCK TABLE ")
            .push(&schema)
            .push(".moderation_member_bans IN ACCESS EXCLUSIVE MODE")
            .build()
            .execute(&mut *locker)
            .await
            .unwrap();
        let store = PgMemberModerationStore::new(pool.clone(), "100000000000000001");
        let discord = MockMemberDiscord::new();
        let svc =
            MemberModerationService::new(discord.clone(), store, policy(), || 1_700_000_000_000);
        let request = execution(action, "query-timeout");
        let error = svc
            .execute(&request)
            .await
            .expect_err("read-only uncertainty query times out");
        assert!(
            matches!(error, two_bot_core::member_moderation::MemberError::Store(ref e)
            if e.is_safe_pre_mutation() && e.message.contains("57014"))
        );
        assert_eq!(discord.call_count("ban"), 0);
        locker.rollback().await.unwrap();
        let rows: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM moderation_member_bans) +
                    (SELECT COUNT(*) FROM moderation_scheduled_unbans) +
                    (SELECT COUNT(*) FROM moderation_idempotency)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows, 0,
            "no-write error releases only the newly claimed key"
        );
        let result = svc
            .execute(&request)
            .await
            .expect("same-key retry after transient read failure");
        assert!(!result.replayed);
        assert_eq!(discord.call_count("ban"), 1);
        assert!(svc.execute(&request).await.unwrap().replayed);
        assert_eq!(discord.call_count("ban"), 1);
        cleanup(admin, pool, schema).await;
    }
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_pre_dispatch_rollback_retries_and_completion_loss_keeps_audit() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "100000000000000001");
    let discord = MockMemberDiscord::new();
    let svc = MemberModerationService::new(discord.clone(), store, policy(), || 1_700_000_000_000);
    sqlx::query("ALTER TABLE moderation_scheduled_unbans ADD CONSTRAINT refuse_stage CHECK (request_id <> 'stage-loss')")
        .execute(&pool).await.expect("scratch stage failure");
    let request = execution(ModerationAction::TempBan, "stage-loss");
    assert!(svc.execute(&request).await.is_err());
    assert_eq!(discord.call_count("ban"), 0);
    let intents: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moderation_member_bans WHERE request_id = 'stage-loss'",
    )
    .fetch_one(&pool)
    .await
    .expect("stage transaction rolled back");
    assert_eq!(intents, 0);
    let claims: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moderation_idempotency WHERE idempotency_key = 'stage-loss'",
    )
    .fetch_one(&pool)
    .await
    .expect("pre-dispatch claim released");
    assert_eq!(claims, 0);
    sqlx::query("ALTER TABLE moderation_scheduled_unbans DROP CONSTRAINT refuse_stage")
        .execute(&pool)
        .await
        .expect("repair scratch stage failure");
    svc.execute(&request)
        .await
        .expect("same-key retry after definite rollback");
    assert_eq!(discord.call_count("ban"), 1);
    sqlx::query("ALTER TABLE moderation_idempotency ADD CONSTRAINT refuse_completion CHECK (state <> 'done') NOT VALID")
        .execute(&pool).await.expect("scratch completion failure");
    let kick = execution(ModerationAction::Kick, "completion-loss");
    assert!(svc.execute(&kick).await.is_err());
    let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moderation_audit WHERE request_id = 'completion-loss' AND outcome = 'kicked'")
        .fetch_one(&pool).await.expect("accepted kick audited before failing completion");
    assert_eq!(audits, 1);
    assert!(matches!(
        svc.execute(&kick).await,
        Err(two_bot_core::member_moderation::MemberError::InFlight)
    ));
    assert_eq!(discord.call_count("kick"), 1);
    cleanup(admin, pool, schema).await;
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_accepted_bans_audit_before_confirmation_and_activation() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "100000000000000001");
    let discord = MockMemberDiscord::new();
    let svc = MemberModerationService::new(discord.clone(), store, policy(), || 1_700_000_000_000);
    sqlx::raw_sql(
        "ALTER TABLE moderation_member_bans ADD CONSTRAINT refuse_confirmation
           CHECK (request_id <> 'confirmation-loss' OR state <> 'accepted');
         ALTER TABLE moderation_scheduled_unbans ADD CONSTRAINT refuse_activation
           CHECK (request_id <> 'activation-loss' OR state <> 'pending');",
    )
    .execute(&pool)
    .await
    .expect("isolated post-acceptance write failures");
    let mut counts = Vec::new();
    for (action, id) in [
        (ModerationAction::Ban, "confirmation-loss"),
        (ModerationAction::TempBan, "activation-loss"),
    ] {
        let mut request = execution(action, id);
        // The first case retains a prepared PUT fence. Exercise activation
        // failure independently on a different member, not through that fence.
        if id == "activation-loss" {
            request.target.as_mut().expect("target").user_id = "444444444444444444".into();
        }
        let error = svc
            .execute(&request)
            .await
            .expect_err("accepted PUT, failed bookkeeping");
        assert!(
            matches!(error, two_bot_core::member_moderation::MemberError::Store(ref e) if e.message.contains("23514"))
        );
        assert!(matches!(
            svc.execute(&request).await,
            Err(two_bot_core::member_moderation::MemberError::InFlight)
        ));
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moderation_audit WHERE request_id = $1 AND outcome = $2",
        )
        .bind(id)
        .bind(if action == ModerationAction::Ban {
            "banned"
        } else {
            "temporarily_banned"
        })
        .fetch_one(&pool)
        .await
        .expect("observed acceptance audit");
        counts.push(count);
    }
    assert_eq!(discord.call_count("ban"), 2);
    assert_eq!(ban_state(&pool, "confirmation-loss").await, "prepared");
    assert_eq!(ban_state(&pool, "activation-loss").await, "accepted");
    assert_eq!(unban_state(&pool, "activation-loss").await, "staged");
    cleanup(admin, pool, schema).await;
    assert_eq!(
        counts,
        vec![1, 1],
        "both observed PUT acceptances must be audited independently"
    );
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_unban_audit_precedes_successful_and_failed_completion() {
    let (admin, pool, schema) = database().await;
    let store = PgMemberModerationStore::new(pool.clone(), "guild");
    let discord = MockMemberDiscord::new();
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    sqlx::raw_sql(
        "CREATE TABLE audit_observations (request_id TEXT, schedule_state TEXT, token_present BOOLEAN);
         CREATE FUNCTION observe_unban_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF NEW.action = 'moderation.unban_scheduled' THEN
             INSERT INTO audit_observations SELECT NEW.request_id, state, claim_token IS NOT NULL
             FROM moderation_scheduled_unbans WHERE request_id = NEW.idempotency_key;
           END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER observe_unban_audit BEFORE INSERT ON moderation_audit
           FOR EACH ROW EXECUTE FUNCTION observe_unban_audit();
         ALTER TABLE moderation_scheduled_unbans ADD CONSTRAINT refuse_completion
           CHECK (request_id <> 'completion-loss' OR state <> 'done');",
    ).execute(&pool).await.expect("observe state at the audit boundary");
    let mut observations = Vec::new();
    for request in ["completion-loss", "success"] {
        accepted_unban(&store, "guild", request, request, NOW, NOW).await;
        let result = svc.run_due_unbans("guild").await;
        if request == "completion-loss" {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), 1);
        }
        let row = sqlx::query(
            "SELECT schedule_state, token_present FROM audit_observations WHERE request_id = $1",
        )
        .bind(format!("{request}:unban"))
        .fetch_one(&pool)
        .await
        .expect("audit attempted for accepted DELETE");
        observations.push((
            row.get::<String, _>("schedule_state"),
            row.get::<bool, _>("token_present"),
        ));
    }
    assert_eq!(unban_state(&pool, "completion-loss").await, "running");
    assert_eq!(unban_state(&pool, "success").await, "done");
    assert_eq!(discord.call_count("unban"), 2);
    cleanup(admin, pool, schema).await;
    assert_eq!(
        observations,
        vec![("running".into(), true), ("running".into(), true)]
    );
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_staging_and_due_claim_share_the_member_queue() {
    let (admin, pool, schema) = database().await;
    let guild = "100000000000000001";
    let user = "333333333333333333";
    let store = PgMemberModerationStore::new(pool.clone(), guild);
    accepted_unban(&store, guild, user, "old-expiry", NOW, NOW).await;
    store
        .activate_staged_unban(guild, user, "old-expiry", NOW)
        .await
        .expect("pending old expiry");
    let key = i64::from(rand::random::<u32>() & 0x7fff_ffff);
    sqlx::raw_sql(
        "CREATE TABLE staging_latch (key BIGINT NOT NULL);
         CREATE FUNCTION pause_prepared_insert() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.request_id = 'queue-race' THEN PERFORM pg_advisory_xact_lock((SELECT key FROM staging_latch)); END IF; RETURN NEW; END $$;
         CREATE TRIGGER pause_prepared_insert BEFORE INSERT ON moderation_member_bans FOR EACH ROW EXECUTE FUNCTION pause_prepared_insert();"
    ).execute(&pool).await.expect("pause staging after the fence check");
    sqlx::query("INSERT INTO staging_latch VALUES ($1)")
        .bind(key)
        .execute(&pool)
        .await
        .expect("random test latch key");
    let mut locker = admin
        .acquire()
        .await
        .expect("scratch advisory-lock connection");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(key)
        .execute(&mut *locker)
        .await
        .expect("hold test latch");
    let discord = MockMemberDiscord::new();
    let svc = Arc::new(MemberModerationService::new(
        discord.clone(),
        store,
        policy(),
        || 1_700_000_000_000,
    ));
    let ban_svc = svc.clone();
    let ban = tokio::spawn(async move {
        ban_svc
            .execute(&execution(ModerationAction::Ban, "queue-race"))
            .await
    });
    let waiting = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND objid::bigint = $1 AND NOT granted)")
                .bind(key).fetch_one(&pool).await.expect("test latch observation");
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.is_ok();
    let sweep_svc = svc.clone();
    let mut sweep = tokio::spawn(async move { sweep_svc.run_due_unbans(guild).await });
    let sweep_waited = tokio::time::timeout(Duration::from_millis(30), &mut sweep)
        .await
        .is_err();
    let before_commit = unban_state(&pool, "old-expiry").await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut *locker)
        .await
        .expect("release only the scratch latch");
    drop(locker);
    let ban_result = tokio::time::timeout(Duration::from_secs(5), ban)
        .await
        .expect("bounded ban")
        .expect("ban task");
    let sweep_result = if sweep_waited {
        Some(
            tokio::time::timeout(Duration::from_secs(5), sweep)
                .await
                .expect("bounded sweep")
                .expect("sweep task"),
        )
    } else {
        None
    };
    let final_state = unban_state(&pool, "old-expiry").await;
    let next = svc
        .execute(&execution(ModerationAction::Ban, "after-race"))
        .await;
    cleanup(admin, pool, schema).await;
    assert!(waiting && sweep_waited);
    assert_eq!(
        before_commit, "pending",
        "a queue waiter must not hold a dispatched claim"
    );
    assert!(ban_result.is_ok());
    assert_eq!(sweep_result.expect("waited sweep").expect("sweep"), 0);
    assert_eq!(final_state, "superseded");
    assert_eq!(discord.call_count("unban"), 0);
    assert!(next.is_ok(), "no permanently running undispatched row");
}

#[tokio::test]
#[ignore = "requires approved agent-testdb or CI Postgres service"]
async fn postgres_void_requeues_current_expiry_and_fence_refusal_releases_only_new_key() {
    use two_bot_core::member_moderation::{MemberError, UnbanResolution};
    let (admin, pool, schema) = database().await;
    let guild = "100000000000000001";
    let user = "333333333333333333";
    let store = PgMemberModerationStore::new(pool.clone(), guild);
    let discord = MockMemberDiscord::new();
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
        1_700_000_000_000
    });
    let mut safe_refusals = Vec::new();
    for (i, action) in [ModerationAction::Ban, ModerationAction::TempBan]
        .into_iter()
        .enumerate()
    {
        let old = format!("old-{i}");
        accepted_unban(&store, guild, user, &old, NOW, NOW).await;
        discord.fail_with("unban", DiscordError::Timeout);
        assert!(matches!(
            svc.run_due_unbans(guild).await,
            Err(MemberError::Discord(DiscordError::Timeout))
        ));
        let token: String = sqlx::query_scalar(
            "SELECT claim_token FROM moderation_scheduled_unbans WHERE request_id = $1",
        )
        .bind(&old)
        .fetch_one(&pool)
        .await
        .unwrap();
        let fresh = execution(action, &format!("fresh-{i}"));
        let error = svc
            .execute(&fresh)
            .await
            .expect_err("old dispatch still fenced");
        safe_refusals.push(matches!(error, MemberError::Store(ref e) if e.is_safe_pre_mutation()));
        assert_eq!(unban_state(&pool, &old).await, "running");
        assert!(store.owns_unban_claim(&old, &token).await.unwrap());
        assert!(store
            .resolve_uncertain_unban(&old, "wrong", UnbanResolution::Void)
            .await
            .is_err());
        store
            .resolve_uncertain_unban(&old, &token, UnbanResolution::Void)
            .await
            .expect("exact DELETE cannot land");
        let pending = unban_state(&pool, &old).await;
        discord.clear_failure("unban");
        let sweep = svc.run_due_unbans(guild).await.unwrap();
        let retried = svc.execute(&fresh).await;
        safe_refusals.push(pending == "pending" && sweep == 1 && retried.is_ok());
    }
    cleanup(admin, pool, schema).await;
    assert_eq!(
        safe_refusals,
        vec![true; 4],
        "preserve expiry and retry the never-dispatched key"
    );
}
