#![cfg(feature = "db")]

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use two_bot_core::member_moderation::{
    ClaimState, DiscordError, MemberExecution, MemberModerationService, MemberModerationStore,
    MockMemberDiscord,
};
use two_bot_core::member_moderation_store::PgMemberModerationStore;
use two_bot_core::{ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget};

const NOW: &str = "2023-11-14T22:13:20.000Z";
const DUE: &str = "2023-11-14T23:13:20.000Z";

// No DATABASE_URL or inherited credentials: tests accept only the approved
// agent container or the ephemeral Postgres service in GitHub Actions.
async fn database() -> (PgPool, PgPool, String) {
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
    let migration = include_str!("../../cutover/migrations/0110_moderation_member.sql");
    sqlx::raw_sql(migration)
        .execute(&pool)
        .await
        .expect("migration");
    sqlx::raw_sql(migration)
        .execute(&pool)
        .await
        .expect("idempotent migration");
    (admin, pool, schema)
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
    let store = PgMemberModerationStore::new(pool.clone());
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
    let svc = MemberModerationService::new(discord.clone(), store.clone(), policy, || {
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
    assert_eq!(svc.run_due_unbans().await.expect("not due"), 0);
    clock.store(1_700_003_600_000, Ordering::SeqCst);
    assert_eq!(svc.run_due_unbans().await.expect("due"), 1);
    assert_eq!(svc.run_due_unbans().await.expect("no duplicate"), 0);
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

    // Crash-left staged expiry is recovered. Parallel sweeps own it once;
    // old tokens cannot complete/requeue a new claim or steal a newer ban.
    store
        .stage_unban("g2", "u2", NOW, "expiry", "crash", NOW)
        .await
        .expect("stage");
    let (a, b) = tokio::join!(
        store.claim_due_unbans(NOW, 25),
        store.claim_due_unbans(NOW, 25)
    );
    let jobs: Vec<_> = a
        .expect("sweep A")
        .into_iter()
        .chain(b.expect("sweep B"))
        .collect();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job.request_id, "crash");
    assert!(store
        .complete_unban(&job.request_id, "wrong-token")
        .await
        .is_err());
    store
        .requeue_unban(&job.request_id, &job.claim_token)
        .await
        .expect("safe retry");
    let next = store
        .claim_due_unbans(NOW, 25)
        .await
        .expect("new claim")
        .pop()
        .expect("job");
    assert_ne!(next.claim_token, job.claim_token);
    assert!(!store
        .owns_unban_claim(&job.request_id, &job.claim_token)
        .await
        .expect("stale owner"));
    assert!(store
        .claim_due_unbans(DUE, 25)
        .await
        .expect("no running takeover")
        .is_empty());
    store
        .stage_unban("g2", "u2", DUE, "extension", "new", NOW)
        .await
        .expect("new stage");
    // Activation with the wrong identity rolls back without supersession.
    assert!(store
        .activate_staged_unban("wrong-guild", "u2", "new", NOW)
        .await
        .is_err());
    assert!(store
        .owns_unban_claim(&next.request_id, &next.claim_token)
        .await
        .expect("old claim preserved"));
    store
        .activate_staged_unban("g2", "u2", "new", NOW)
        .await
        .expect("new activation");
    assert!(!store
        .owns_unban_claim(&next.request_id, &next.claim_token)
        .await
        .expect("superseded"));
    assert!(store
        .complete_unban(&next.request_id, &next.claim_token)
        .await
        .is_err());
    assert_eq!(
        store
            .claim_due_unbans(DUE, 25)
            .await
            .expect("new expiry")
            .len(),
        1
    );

    store
        .stage_unban("g3", "u3", NOW, "old expiry", "old-staged", NOW)
        .await
        .expect("old crash");
    store
        .stage_unban(
            "g3",
            "u3",
            DUE,
            "extension",
            "new-staged",
            "2023-11-14T22:13:21.000Z",
        )
        .await
        .expect("new crash");
    assert!(store
        .claim_due_unbans(NOW, 25)
        .await
        .expect("no early recovery")
        .is_empty());
    let recovered = store
        .claim_due_unbans(DUE, 25)
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
